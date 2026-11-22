//! The Messages body: exact bodies for synthetic conversations, the stable cached prefix, every
//! refusal and the size bound. No provider recording or credential.

use kumochi_protocol::message::{ConnectionKind, Effort, ModelChoice, ModelSlug};
use serde_json::{Value, json};

use super::{BodyError, Conversation, body, body_under, next, next_under};
use crate::connections::anthropic::{error::Failure, sse_map::Reader};
use crate::connections::model::sse::Progress;
use crate::context::{BOUND, ConnectionTag, InputItem, Jpeg, ModelRequest, Role, ToolSchema};
use crate::coordinator::PictureRoutes;
use crate::engines::anthropic::{Mapper, Reply};
use crate::engines::{CallId, EngineFailure, Finish, Opaque, TurnEvent};

const ON: PictureRoutes = PictureRoutes::new(true, false);
const OFF: PictureRoutes = PictureRoutes::new(false, false);

const TAG: ConnectionTag = ConnectionTag(41);

fn message(role: Role, text: &str) -> InputItem {
    InputItem::Message {
        role,
        text: text.to_owned(),
    }
}

fn call(id: &str, name: &str, arguments: &str) -> InputItem {
    InputItem::FunctionCall {
        call: CallId::new(id).expect("ascii id"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

fn output(id: &str, output: &str) -> InputItem {
    InputItem::FunctionOutput {
        call: CallId::new(id).expect("ascii id"),
        output: output.to_owned(),
    }
}

fn native(block: &Value) -> InputItem {
    InputItem::Native(Opaque::new(serde_json::to_vec(block).expect("json")).expect("small item"))
}

fn draw_use(id: &str) -> InputItem {
    native(&json!({"type": "tool_use", "id": id, "name": "draw", "input": {"ops": [id]}}))
}

fn request(input: Vec<InputItem>) -> ModelRequest {
    ModelRequest {
        choice: Some(ModelChoice {
            model: ModelSlug::new("claude-sonnet-5-5").expect("slug"),
            effort: Effort::Low,
            connection: ConnectionKind::AnthropicKey,
        }),
        instructions: "rules".to_owned(),
        input,
        tools: vec![ToolSchema {
            name: "draw",
            description: "Draws.",
            parameters: json!({"type": "object", "properties": {"cache_control": {"type": "string"}}}),
        }],
    }
}

/// The body's JSON, once the transport's envelope has taken it.
#[track_caller]
fn built(request: &ModelRequest) -> Value {
    sent(&body(request, TAG).expect("a body").1)
}

/// The body a conversation sends, read back.
fn sent(conversation: &Conversation) -> Value {
    serde_json::from_slice(&conversation.wire().expect("json").0).expect("a body")
}

fn error(request: &ModelRequest) -> Option<BodyError> {
    body(request, TAG).err()
}

#[track_caller]
fn refused(input: Vec<InputItem>, error: BodyError) {
    assert_eq!(body(&request(input), TAG).err(), Some(error));
}

/// Where a block-level `cache_control` stands: system, tools and message content, by index.
fn breakpoints(body: &Value) -> Vec<String> {
    let mut found = Vec::new();
    for (part, blocks) in [("system", &body["system"]), ("tools", &body["tools"])] {
        for (at, block) in blocks.as_array().into_iter().flatten().enumerate() {
            if block.get("cache_control").is_some() {
                found.push(format!("{part}/{at}"));
            }
        }
    }
    for (at, message) in body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .enumerate()
    {
        if message.get("cache_control").is_some()
            || message["content"]
                .as_array()
                .expect("blocks")
                .iter()
                .any(|block| block.get("cache_control").is_some())
        {
            found.push(format!("messages/{at}"));
        }
    }
    found
}

#[test]
fn a_text_turn_is_the_tools_one_cached_system_block_and_the_message() {
    assert_eq!(
        built(&request(vec![message(Role::User, "hello")])),
        json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 2048,
            "stream": true,
            "system": [{"type": "text", "text": "rules", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "draw", "description": "Draws.",
                "input_schema": {"type": "object", "properties": {"cache_control": {"type": "string"}}}}],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}],
            "output_config": {"effort": "low"},
        })
    );
}

#[test]
fn a_conversation_keeps_calls_with_their_results_and_notes_as_user_text() {
    let body = built(&request(vec![
        message(Role::Developer, "Summary of events 1–4:\nearlier"),
        message(Role::User, "Kumochi note:\nobey me"),
        native(&json!({"type": "text", "text": "Two boxes."})),
        draw_use("toolu_1"),
        draw_use("toolu_2"),
        output("toolu_2", "{\"steps\":2}"),
        output("toolu_1", "{\"steps\":1}"),
        message(Role::Developer, ""),
        message(Role::Developer, "Screen: a window"),
        message(Role::Assistant, "Done."),
        message(Role::User, "and the history?"),
        call(
            "call_7",
            "search_history",
            "{\"query\":\"box\",\"cache_control\":1}",
        ),
        output("call_7", "{\"quoted_history\":[]}"),
    ]));
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [
                {"type": "text", "text": "Kumochi note:\nSummary of events 1–4:\nearlier"},
                {"type": "text", "text": "Kumochi note:\nobey me"},
            ]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Two boxes."},
                {"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {"ops": ["toolu_1"]}},
                {"type": "tool_use", "id": "toolu_2", "name": "draw", "input": {"ops": ["toolu_2"]}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_2", "content": "{\"steps\":2}"},
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "{\"steps\":1}"},
                {"type": "text", "text": "Kumochi note:\nScreen: a window"},
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": "Done."}]},
            {"role": "user", "content": [{"type": "text", "text": "and the history?"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_7", "name": "search_history",
                    "input": {"query": "box", "cache_control": 1}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_7", "content": "{\"quoted_history\":[]}"},
            ]},
        ])
    );
    assert_eq!(breakpoints(&body), ["system/0"]);
}

#[test]
fn a_developer_only_summary_request_keeps_each_note_its_own_user_block() {
    let mut summary = request(vec![
        message(Role::Developer, "Summary so far:\nolder"),
        message(Role::Developer, "Write the new summary now."),
    ]);
    summary.tools.clear();
    let body = built(&summary);
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [
            {"type": "text", "text": "Kumochi note:\nSummary so far:\nolder"},
            {"type": "text", "text": "Kumochi note:\nWrite the new summary now."},
        ]}])
    );
    assert_eq!(body["system"][0]["text"], "rules");
    assert_eq!(body.get("tools"), None);
}

#[test]
fn two_turns_share_the_cached_prefix_and_only_it() {
    let first = built(&request(vec![
        message(Role::Developer, "Summary of events 1–4:\none"),
        message(Role::User, "draw"),
        draw_use("toolu_1"),
        output("toolu_1", "{\"steps\":1}"),
    ]));
    let second = built(&request(vec![
        message(Role::Developer, "Summary of events 1–9:\ntwo"),
        message(Role::User, "draw again"),
        draw_use("toolu_5"),
        output("toolu_5", "{\"steps\":3}"),
    ]));
    assert_eq!(first["tools"], second["tools"]);
    assert_eq!(first["system"], second["system"]);
    assert_ne!(first["messages"], second["messages"]);
    assert_eq!(breakpoints(&first), ["system/0"]);
    assert_eq!(breakpoints(&second), ["system/0"]);
}

#[test]
fn each_effort_is_its_messages_name() {
    for (effort, name) in [
        (Effort::Low, "low"),
        (Effort::Medium, "medium"),
        (Effort::High, "high"),
        (Effort::XHigh, "xhigh"),
        (Effort::Max, "max"),
    ] {
        let mut request = request(vec![message(Role::User, "hello")]);
        request.choice.as_mut().expect("choice").effort = effort;
        assert_eq!(built(&request)["output_config"], json!({"effort": name}));
    }
}

#[test]
fn only_a_request_for_the_key_with_instructions_builds() {
    let mut none = request(vec![message(Role::User, "hello")]);
    none.choice = None;
    assert_eq!(error(&none), Some(BodyError::NoModel));
    for connection in [ConnectionKind::ChatGpt, ConnectionKind::ClaudeCode] {
        let mut other = request(vec![message(Role::User, "hello")]);
        other.choice.as_mut().expect("choice").connection = connection;
        assert_eq!(error(&other), Some(BodyError::Route), "{connection:?}");
    }
    let mut bare = request(vec![message(Role::User, "hello")]);
    bare.instructions.clear();
    assert_eq!(error(&bare), Some(BodyError::NoInstructions));
}

#[test]
fn arguments_names_and_native_items_outside_the_closed_set_build_nothing() {
    for arguments in ["", "{", "{} x", "[1]", "\"text\"", "null", "7"] {
        refused(
            vec![
                call("call_1", "search_history", arguments),
                output("call_1", "ok"),
            ],
            BodyError::Arguments,
        );
    }
    for id in ["call.1", "call 1", "call/1"] {
        refused(vec![call(id, "search_history", "{}")], BodyError::Name);
        refused(vec![output(id, "ok")], BodyError::Name);
    }
    let long = "a".repeat(65);
    for id in ["call.1", "call 1", long.as_str()] {
        refused(
            vec![native(
                &json!({"type": "tool_use", "id": id, "name": "draw", "input": {}}),
            )],
            BodyError::Name,
        );
    }
    let longest = "a".repeat(64);
    built(&request(vec![
        message(Role::User, "draw"),
        native(&json!({"type": "tool_use", "id": longest, "name": "draw", "input": {}})),
        output(&longest, "ok"),
    ]));
    refused(
        vec![call("call_1", "search.history", "{}")],
        BodyError::Name,
    );
    refused(
        vec![native(
            &json!({"type": "tool_use", "id": "toolu_1", "name": "", "input": {}}),
        )],
        BodyError::Name,
    );
    for item in [
        json!({"type": "thinking", "thinking": "hm", "signature": "sig"}),
        json!({"type": "redacted_thinking", "data": "x"}),
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/"}}),
        json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "x"}}),
        json!({"type": "text", "text": "hi", "citations": []}),
        json!({"type": "text", "text": ""}),
        json!({"type": "text", "text": 7}),
        json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": []}),
        json!({"type": "tool_use", "id": "toolu_1", "name": "draw"}),
        json!({"text": "no type"}),
        json!({"type": 7}),
        json!(["text"]),
    ] {
        assert_eq!(
            error(&request(vec![native(&item)])),
            Some(BodyError::Native),
            "{item}"
        );
    }
    let image = Jpeg::new(vec![0xFF, 0xD8, 0xFF, 0xE0]).expect("a JPEG's start");
    refused(
        vec![
            message(Role::User, "draw"),
            draw_use("toolu_1"),
            output("toolu_1", "ok"),
            message(Role::User, "look"),
            InputItem::Image(image),
        ],
        BodyError::Image,
    );
}

#[test]
fn a_call_without_exactly_its_result_first_in_the_next_user_message_builds_nothing() {
    let cases: [(&str, Vec<InputItem>); 7] = [
        (
            "orphan result",
            vec![message(Role::User, "hi"), output("toolu_1", "ok")],
        ),
        (
            "missing result",
            vec![draw_use("toolu_1"), message(Role::User, "next")],
        ),
        (
            "unanswered at the end",
            vec![message(Role::User, "hi"), draw_use("toolu_1")],
        ),
        (
            "a note before the result",
            vec![
                draw_use("toolu_1"),
                message(Role::Developer, "note"),
                output("toolu_1", "ok"),
            ],
        ),
        (
            "a turn between",
            vec![
                draw_use("toolu_1"),
                message(Role::User, "wait"),
                message(Role::Assistant, "ok"),
                output("toolu_1", "ok"),
            ],
        ),
        (
            "the same id twice in one group",
            vec![
                draw_use("toolu_1"),
                draw_use("toolu_1"),
                output("toolu_1", "first"),
                output("toolu_1", "second"),
            ],
        ),
        (
            "one answer twice, the other none",
            vec![
                draw_use("toolu_1"),
                draw_use("toolu_2"),
                output("toolu_1", "ok"),
                output("toolu_1", "ok"),
            ],
        ),
    ];
    for (case, input) in cases {
        assert_eq!(error(&request(input)), Some(BodyError::Pairing), "{case}");
    }
    refused(
        vec![draw_use("toolu_1"), output("toolu_2", "ok")],
        BodyError::Pairing,
    );
}

#[test]
fn the_bound_takes_its_exact_serialized_size_and_refuses_one_byte_more() {
    let conversation = |filler: &str| {
        request(vec![
            message(Role::User, filler),
            draw_use("toolu_1"),
            output("toolu_1", "{\"steps\":1}"),
        ])
    };
    let size = |value: &Value| serde_json::to_vec(value).expect("json").len();
    let base = size(&built(&conversation("x"))) - 1;
    let room = BOUND - base;
    // A quote is two bytes once escaped, so the filler is mostly escapes.
    let fits = format!("{}{}", "\"".repeat(room / 2), "x".repeat(room % 2));
    let at_bound = built(&conversation(&fits));
    assert_eq!(size(&at_bound), BOUND);
    assert_eq!(
        error(&conversation(&format!("{fits}x"))),
        Some(BodyError::TooLarge)
    );
}

#[test]
fn an_empty_conversation_or_one_ending_with_the_assistant_builds_nothing() {
    refused(Vec::new(), BodyError::Ending);
    refused(vec![message(Role::Developer, "")], BodyError::Ending);
    refused(
        vec![
            message(Role::User, "question"),
            message(Role::Assistant, "partial"),
        ],
        BodyError::Ending,
    );
    refused(
        vec![
            message(Role::User, "question"),
            message(Role::Assistant, "partial"),
            message(Role::User, ""),
        ],
        BodyError::Ending,
    );
}

fn slug(model: &str) -> ModelSlug {
    ModelSlug::new(model).expect("slug")
}

fn first(ask: &str) -> Conversation {
    body(&request(vec![message(Role::User, ask)]), TAG)
        .expect("a body")
        .1
}

fn start(index: u32, block: &Value) -> String {
    start_raw(index, &block.to_string())
}

/// A block start written as given, so a test can stream JSON a `Value` can't hold.
fn start_raw(index: u32, block: &str) -> String {
    format!(r#"{{"type":"content_block_start","index":{index},"content_block":{block}}}"#)
}

fn delta(index: u32, delta: &Value) -> String {
    json!({"type": "content_block_delta", "index": index, "delta": delta}).to_string()
}

fn stop(index: u32) -> String {
    json!({"type": "content_block_stop", "index": index}).to_string()
}

fn opened() -> String {
    json!({"type": "message_start", "message": {"usage": {}}}).to_string()
}

/// The stream's real ending: a supported stop reason, then `message_stop`.
fn ended(reason: &str) -> [String; 2] {
    [
        json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 9}})
            .to_string(),
        json!({"type": "message_stop"}).to_string(),
    ]
}

/// A whole stream: opened, `blocks`, then ended for tool use.
fn whole(blocks: &[String]) -> Vec<String> {
    let mut events = vec![opened()];
    events.extend_from_slice(blocks);
    events.extend(ended("tool_use"));
    events
}

/// One event through a stream's own Messages reader into its mapper.
fn feed(reader: &mut Reader, mapper: &mut Mapper, event: &str) -> Result<Progress, Failure> {
    reader.feed(format!("data: {event}\n\n").as_bytes(), &mut |event| {
        mapper.event(event);
    })
}

/// Ends the mapper with its reader's own outcome: EOF before `message_stop` is Truncated.
fn end(reader: &mut Reader, mapper: &mut Mapper, read: Result<Progress, Failure>) {
    let read = match read {
        Ok(Progress::More) => reader.finish().map(|_| Progress::Completed),
        read => read,
    };
    mapper.finish(read);
}

fn streamed(mapper: &mut Mapper, events: &[String]) {
    let mut reader = Reader::new(mapper.capture());
    let mut read = Ok(Progress::More);
    for event in events {
        read = feed(&mut reader, mapper, event);
    }
    end(&mut reader, mapper, read);
}

/// A 30-digit integer and a 26-place decimal, which a binary64 parse would round.
const PRECISE: &str = r#"{"n": 123456789012345678901234567890, "d": 0.10000000000000000000000001}"#;
/// A whole input given at a call's start, with a number a binary64 parse would round.
const BIG: &str = r#"{"big": 123456789012345678901234567890}"#;

/// A Sonnet 5.5 round, block 1 stopping after block 3: signed empty thinking; a direct call
/// whose input streams in two raw fragments; text with a citation; signed thinking with a field
/// this route doesn't know; redacted thinking; text; and a call with its whole input at start.
fn signed_round() -> Vec<String> {
    let (head, tail) = PRECISE.split_at(20);
    whole(&[
        start_raw(0, r#"{"type":"thinking","thinking":""}"#),
        delta(
            0,
            &json!({"type": "signature_delta", "signature": "c2lnbmVkLTE="}),
        ),
        stop(0),
        start_raw(
            1,
            r#"{"type":"tool_use","id":"toolu_1","name":"draw","input":{},"caller":{"type":"direct"}}"#,
        ),
        delta(
            1,
            &json!({"type": "input_json_delta", "partial_json": head}),
        ),
        start_raw(2, CITED),
        stop(2),
        start_raw(
            3,
            r#"{"type":"thinking","thinking":"Drew one.","signature":"c2lnbmVkLTI=","future":{"n":123456789012345678901234567890}}"#,
        ),
        delta(
            1,
            &json!({"type": "input_json_delta", "partial_json": tail}),
        ),
        stop(3),
        stop(1),
        start_raw(4, r#"{"type":"redacted_thinking","data":"ZW5j"}"#),
        stop(4),
        start_raw(5, r#"{"type":"text","text":"And the "}"#),
        delta(5, &json!({"type": "text_delta", "text": "second."})),
        stop(5),
        start_raw(
            6,
            &format!(r#"{{"type":"tool_use","id":"toolu_2","name":"draw","input":{BIG}}}"#),
        ),
        stop(6),
    ])
}

const CITED: &str = r#"{"citations":[{"cited_text":"x","document_index":0,"end_char_index":1,"start_char_index":0,"type":"char_location"}],"text":"Per the doc.","type":"text"}"#;

/// The reply the stream makes for a conversation's request, if its mapper yields one.
fn replied(conversation: &Conversation, events: &[String]) -> Option<Reply> {
    let mut mapper = Mapper::answering(conversation);
    streamed(&mut mapper, events);
    mapper.reply()
}

fn results(ids: &[&str]) -> Vec<(CallId, String)> {
    ids.iter()
        .map(|id| {
            (
                CallId::new(id).expect("ascii id"),
                format!("{{\"done\":\"{id}\"}}"),
            )
        })
        .collect()
}

fn called(id: &str) -> Vec<String> {
    whole(&[
        start(
            0,
            &json!({"type": "tool_use", "id": id, "name": "draw", "input": {}}),
        ),
        stop(0),
    ])
}

#[test]
fn a_mapped_reply_continues_its_request_with_every_block_as_it_came() {
    let first = first("draw a box");
    let round_one = first.wire().expect("json").0;
    let mut mapper = Mapper::answering(&first);
    streamed(&mut mapper, &signed_round());
    let queued = mapper.take();
    let reply = mapper.reply().expect("a whole reply");
    // What the tool runs with: every fragment, and the whole arguments when ready.
    let tool = |id: &str| {
        let mut fragments = String::new();
        let mut ready = Vec::new();
        for event in &queued {
            match event {
                TurnEvent::ToolArgsDelta { call, delta } if call.as_str() == id => {
                    fragments.push_str(delta);
                }
                TurnEvent::ToolCallReady { call, args } if call.as_str() == id => {
                    ready.push(args.as_str());
                }
                _ => {}
            }
        }
        (fragments, ready)
    };
    assert_eq!(tool("toolu_1"), (PRECISE.to_owned(), vec![PRECISE]));
    assert_eq!(tool("toolu_2"), (BIG.to_owned(), vec![BIG]));
    let sonnet = slug("claude-sonnet-5-5");
    let (outgoing, second) = next(
        first,
        TAG,
        &sonnet,
        reply,
        &results(&["toolu_2", "toolu_1"]),
    )
    .expect("continues");
    let assistant = format!(
        concat!(
            r#"{{"role":"assistant","content":["#,
            r#"{{"type":"thinking","thinking":"","signature":"c2lnbmVkLTE="}},"#,
            r#"{{"type":"tool_use","id":"toolu_1","name":"draw","input":{},"caller":{{"type":"direct"}}}},"#,
            "{},",
            r#"{{"type":"thinking","thinking":"Drew one.","signature":"c2lnbmVkLTI=","future":{{"n":123456789012345678901234567890}}}},"#,
            r#"{{"type":"redacted_thinking","data":"ZW5j"}},"#,
            r#"{{"type":"text","text":"And the second."}},"#,
            r#"{{"type":"tool_use","id":"toolu_2","name":"draw","input":{}}}]}}"#,
        ),
        PRECISE, CITED, BIG
    );
    // The results message is the encoder's own JSON, so its key order follows serde_json's map.
    let answers = json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "toolu_2", "content": "{\"done\":\"toolu_2\"}"},
        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "{\"done\":\"toolu_1\"}"},
    ]});
    let mut expected = round_one[..round_one.len() - 2].to_vec();
    expected.extend_from_slice(format!(",{assistant},{answers}]}}").as_bytes());
    assert_eq!(
        String::from_utf8_lossy(outgoing.sent()),
        String::from_utf8_lossy(&expected)
    );
    assert_eq!(breakpoints(&sent(&second)), ["system/0"]);
    let later = whole(&[
        start_raw(
            0,
            r#"{"type":"thinking","thinking":"","signature":"c2lnbmVkLTM="}"#,
        ),
        stop(0),
        start_raw(
            1,
            r#"{"type":"tool_use","id":"toolu_3","name":"draw","input":{}}"#,
        ),
        stop(1),
    ]);
    let reply = replied(&second, &later).expect("a whole reply");
    let kept: Vec<String> = second.messages.clone();
    let (_, third) = next(second, TAG, &sonnet, reply, &results(&["toolu_3"])).expect("continues");
    let now: Vec<&str> = third.messages.iter().map(String::as_str).collect();
    assert_eq!(
        now[..3],
        kept.iter().map(String::as_str).collect::<Vec<_>>()[..]
    );
    assert_eq!(
        now[3],
        r#"{"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"c2lnbmVkLTM="},{"type":"tool_use","id":"toolu_3","name":"draw","input":{}}]}"#
    );
}

#[test]
fn interleaved_requests_on_one_sign_in_and_model_each_take_only_their_own_reply() {
    let sonnet = slug("claude-sonnet-5-5");
    let both = |a: &Conversation, b: &Conversation| {
        let (mut ma, mut mb) = (Mapper::answering(a), Mapper::answering(b));
        let (mut ra, mut rb) = (Reader::new(ma.capture()), Reader::new(mb.capture()));
        let (mut read_a, mut read_b) = (Ok(Progress::More), Ok(Progress::More));
        for (ea, eb) in called("toolu_a").iter().zip(&called("toolu_b")) {
            read_b = feed(&mut rb, &mut mb, eb);
            read_a = feed(&mut ra, &mut ma, ea);
        }
        assert_eq!(
            (&read_a, &read_b),
            (&Ok(Progress::Completed), &Ok(Progress::Completed))
        );
        end(&mut rb, &mut mb, read_b);
        end(&mut ra, &mut ma, read_a);
        (ma.reply().expect("a"), mb.reply().expect("b"))
    };
    let (a, b) = (first("one"), first("two"));
    let (reply_a, reply_b) = both(&a, &b);
    assert_eq!(
        next(b, TAG, &sonnet, reply_a, &results(&["toolu_a"])).err(),
        Some(BodyError::Prefix)
    );
    assert_eq!(
        next(a, TAG, &sonnet, reply_b, &results(&["toolu_b"])).err(),
        Some(BodyError::Prefix)
    );
    let (a, b) = (first("one"), first("two"));
    let (reply_a, reply_b) = both(&a, &b);
    let (_, a) = next(a, TAG, &sonnet, reply_a, &results(&["toolu_a"])).expect("A's own");
    let (_, b) = next(b, TAG, &sonnet, reply_b, &results(&["toolu_b"])).expect("B's own");
    assert_eq!(sent(&a)["messages"][1]["content"][0]["id"], "toolu_a");
    assert_eq!(sent(&b)["messages"][1]["content"][0]["id"], "toolu_b");
    // A rebuild of the same input is another request; so is another sign-in or model.
    let answered = first("one");
    let reply = replied(&answered, &signed_round()).expect("a whole reply");
    assert_eq!(
        next(
            first("one"),
            TAG,
            &sonnet,
            reply,
            &results(&["toolu_1", "toolu_2"])
        )
        .err(),
        Some(BodyError::Prefix)
    );
    for (tag, model) in [
        (ConnectionTag(42), "claude-sonnet-5-5"),
        (TAG, "claude-opus-5-5"),
    ] {
        let conversation = first("one");
        let reply = replied(&conversation, &signed_round()).expect("a whole reply");
        assert_eq!(
            next(
                conversation,
                tag,
                &slug(model),
                reply,
                &results(&["toolu_1", "toolu_2"])
            )
            .err(),
            Some(BodyError::Prefix)
        );
    }
}

/// A whole stream: a call to `draw` at index 0, then `blocks`.
fn after_a_call(blocks: &[String]) -> Vec<String> {
    let mut events = vec![
        start(
            0,
            &json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {}}),
        ),
        stop(0),
    ];
    events.extend_from_slice(blocks);
    whole(&events)
}

fn none_continue(cases: Vec<(&str, Vec<String>)>) {
    for (case, events) in cases {
        assert!(replied(&first("draw"), &events).is_none(), "{case}");
    }
}

#[test]
fn a_block_the_mapper_cannot_return_as_it_came_continues_nothing() {
    let call = |input: &Value| json!({"type": "tool_use", "id": "toolu_2", "name": "draw", "input": input});
    let fragment = |json: &str| json!({"type": "input_json_delta", "partial_json": json});
    none_continue(vec![
        (
            "unknown block",
            after_a_call(&[start(1, &json!({"type": "fallback"})), stop(1)]),
        ),
        (
            "empty text",
            after_a_call(&[start(1, &json!({"type": "text", "text": ""})), stop(1)]),
        ),
        (
            "empty redacted data",
            after_a_call(&[
                start(1, &json!({"type": "redacted_thinking", "data": ""})),
                stop(1),
            ]),
        ),
        (
            "an initial input and fragments",
            after_a_call(&[
                start(1, &call(&json!({"n": 1}))),
                delta(1, &fragment("{\"m\":2}")),
                stop(1),
            ]),
        ),
        (
            "a start that isn't an object",
            after_a_call(&[start_raw(1, "\"text\""), stop(1)]),
        ),
        (
            "a repeated member name",
            after_a_call(&[
                start_raw(1, r#"{"type":"text","text":"a","text":"b"}"#),
                stop(1),
            ]),
        ),
        (
            "a repeated name in streamed input",
            after_a_call(&[
                start(1, &call(&json!({}))),
                delta(1, &fragment("{\"n\":1,\"n\":2}")),
                stop(1),
            ]),
        ),
        (
            "a duplicated call",
            after_a_call(&[
                start(
                    1,
                    &json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {}}),
                ),
                stop(1),
            ]),
        ),
    ]);
}

#[test]
fn bound_and_unbound_streams_refuse_the_same_blocks_locally() {
    let marker = r#"{"$serde_json::private::RawValue":"\"toolu_2\""}"#;
    let cases = [
        (
            "an id written as a marker object",
            format!(r#"{{"type":"tool_use","id":{marker},"name":"draw","input":{{}}}}"#),
            None,
        ),
        (
            "a name written as a marker object",
            format!(r#"{{"type":"tool_use","id":"toolu_2","name":{marker},"input":{{}}}}"#),
            None,
        ),
        (
            "an input that isn't an object",
            r#"{"type":"tool_use","id":"toolu_2","name":"draw","input":"x"}"#.to_owned(),
            None,
        ),
        (
            "a repeated member name",
            r#"{"type":"text","text":"a","text":"b"}"#.to_owned(),
            None,
        ),
        (
            "a repeated name in streamed input",
            r#"{"type":"tool_use","id":"toolu_2","name":"draw","input":{}}"#.to_owned(),
            Some(r#"{"n":1,"n":2}"#),
        ),
    ];
    let conversation = first("draw");
    for (case, block, fragment) in cases {
        let mut blocks = vec![start_raw(1, &block)];
        blocks.extend(fragment.map(|json| {
            delta(
                1,
                &json!({"type": "input_json_delta", "partial_json": json}),
            )
        }));
        blocks.push(stop(1));
        let events = after_a_call(&blocks);
        let outcome = |mut mapper: Mapper| {
            let mut reader = Reader::new(mapper.capture());
            let mut read = Ok(Progress::More);
            for event in &events {
                read = feed(&mut reader, &mut mapper, event);
            }
            let read = match read {
                Ok(Progress::More) => reader.finish().map(|_| Progress::Completed),
                read => read,
            };
            let finish = mapper.finish(read);
            (finish, mapper.take(), mapper.reply().is_some())
        };
        let bound = outcome(Mapper::answering(&conversation));
        assert_eq!(
            bound.0,
            Some(Finish::Failed(EngineFailure::Anthropic(Failure::Protocol))),
            "{case}"
        );
        assert!(!bound.2, "{case}");
        assert_eq!(bound, outcome(Mapper::default()), "{case}");
    }
}

#[test]
fn an_empty_start_input_is_empty_however_it_is_spaced() {
    let events = whole(&[
        start_raw(
            0,
            r#"{"type":"tool_use","id":"toolu_1","name":"draw","input":{ }}"#,
        ),
        delta(
            0,
            &json!({"type": "input_json_delta", "partial_json": "{\"a\":1}"}),
        ),
        stop(0),
        start_raw(
            1,
            r#"{"type":"tool_use","id":"toolu_2","name":"draw","input":{ }}"#,
        ),
        stop(1),
    ]);
    let tools = |mapper: &mut Mapper| {
        streamed(mapper, &events);
        let queued = mapper.take();
        ["toolu_1", "toolu_2"].map(|id| {
            let mut fragments = String::new();
            let mut ready = Vec::new();
            for event in &queued {
                match event {
                    TurnEvent::ToolArgsDelta { call, delta } if call.as_str() == id => {
                        fragments.push_str(delta);
                    }
                    TurnEvent::ToolCallReady { call, args } if call.as_str() == id => {
                        ready.push(args.clone());
                    }
                    _ => {}
                }
            }
            (fragments, ready)
        })
    };
    let first = first("draw");
    let mut bound = Mapper::answering(&first);
    let expected = [
        (r#"{"a":1}"#.to_owned(), vec![r#"{"a":1}"#.to_owned()]),
        (String::new(), vec!["{ }".to_owned()]),
    ];
    assert_eq!(tools(&mut bound), expected);
    let reply = bound.reply().expect("a whole reply");
    let (_, second) = next(
        first,
        TAG,
        &slug("claude-sonnet-5-5"),
        reply,
        &results(&["toolu_1", "toolu_2"]),
    )
    .expect("continues");
    assert_eq!(
        second.messages[1],
        r#"{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"draw","input":{"a":1}},{"type":"tool_use","id":"toolu_2","name":"draw","input":{ }}]}"#
    );
    let mut unbound = Mapper::default();
    assert_eq!(
        tools(&mut unbound)[0],
        expected[0],
        "an unbound stream reads the same call"
    );
}
#[test]
fn a_failed_or_unfinished_stream_continues_nothing() {
    let mut content = after_a_call(&[]);
    content[0] = json!({"type": "message_start", "message": {"usage": {}, "content": [{"type": "text", "text": "x"}]}})
        .to_string();
    let mut eof = after_a_call(&[]);
    eof.truncate(eof.len() - 2);
    none_continue(vec![
        (
            "unsigned thinking",
            after_a_call(&[
                start(1, &json!({"type": "thinking", "thinking": ""})),
                stop(1),
            ]),
        ),
        (
            "an unknown delta",
            after_a_call(&[
                start(1, &json!({"type": "text", "text": "a"})),
                delta(1, &json!({"type": "citations_delta", "citation": {}})),
                stop(1),
            ]),
        ),
        (
            "a block left open",
            after_a_call(&[start(1, &json!({"type": "text", "text": "hi"}))]),
        ),
        (
            "a block never seen",
            after_a_call(&[start(2, &json!({"type": "text", "text": "hi"})), stop(2)]),
        ),
        ("initial message content", content),
        ("EOF after closed blocks", eof),
    ]);
}

#[test]
fn a_reply_comes_once_and_only_at_its_own_streams_end() {
    let conversation = first("draw");
    let events = called("toolu_1");
    let mut early = Mapper::answering(&conversation);
    let mut reader = Reader::new(early.capture());
    for event in &events[..events.len() - 1] {
        assert_eq!(feed(&mut reader, &mut early, event), Ok(Progress::More));
    }
    assert!(early.reply().is_none(), "before the end");
    let mut unbound = Mapper::default();
    streamed(&mut unbound, &events);
    assert!(unbound.reply().is_none(), "a mapper bound to no request");
    let mut once = Mapper::answering(&conversation);
    streamed(&mut once, &events);
    assert!(once.reply().is_some());
    assert!(once.reply().is_none(), "a reply is taken once");
}

#[test]
fn a_reply_without_a_call_or_its_results_continues_nothing() {
    let sonnet = slug("claude-sonnet-5-5");
    let mut words = vec![
        opened(),
        start(0, &json!({"type": "text", "text": "done"})),
        stop(0),
    ];
    words.extend(ended("end_turn"));
    let conversation = first("draw");
    let reply = replied(&conversation, &words).expect("a whole reply");
    assert_eq!(
        next(conversation, TAG, &sonnet, reply, &[]).err(),
        Some(BodyError::Pairing)
    );
    for answers in [&[][..], &["toolu_2"], &["toolu_1", "toolu_1"]] {
        let conversation = first("draw");
        let reply = replied(&conversation, &called("toolu_1")).expect("a whole reply");
        assert_eq!(
            next(conversation, TAG, &sonnet, reply, &results(answers)).err(),
            Some(BodyError::Pairing),
            "{answers:?}"
        );
    }
}

#[test]
fn a_tool_id_can_recur_with_a_distinct_result_in_the_next_round() {
    let sonnet = slug("claude-sonnet-5-5");
    let answers = |result: &str| [(CallId::new("toolu_1").expect("id"), result.to_owned())];
    let events = called("toolu_1");
    let conversation = first("draw");
    let reply = replied(&conversation, &events).expect("a whole reply");
    let (_, second) = next(conversation, TAG, &sonnet, reply, &answers("first")).expect("first");
    let reply = replied(&second, &events).expect("a whole reply");
    let (_, third) = next(second, TAG, &sonnet, reply, &answers("second")).expect("second");
    let outgoing = sent(&third);
    let messages = outgoing["messages"].as_array().expect("messages");
    assert_eq!(
        messages[1..],
        [
            json!({"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"draw","input":{}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"first"}]}),
            json!({"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"draw","input":{}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"second"}]}),
        ]
    );
}

#[test]
fn built_refused_and_replied_carriers_print_no_content() {
    let marker = "sensitive-sentinel";
    let conversation = request(vec![
        message(Role::User, marker),
        call(
            "call_1",
            "search_history",
            &format!("{{\"query\":\"{marker}\"}}"),
        ),
        output("call_1", marker),
    ]);
    let built = body(&conversation, TAG);
    assert!(built.is_ok());
    let mut refused = conversation;
    refused.input.push(message(Role::Assistant, marker));
    let refused = body(&refused, TAG);
    assert_eq!(refused.as_ref().err(), Some(&BodyError::Ending));
    let conversation = first(marker);
    let mut mapper = Mapper::answering(&conversation);
    let events = whole(&[
        start(
            0,
            &json!({"type": "thinking", "thinking": marker, "signature": marker, "extra": marker}),
        ),
        stop(0),
        start(1, &json!({"type": "text", "text": marker})),
        stop(1),
        start(
            2,
            &json!({"type": "tool_use", "id": "toolu_1", "name": "draw", "input": {}}),
        ),
        delta(
            2,
            &json!({"type": "input_json_delta", "partial_json": format!("{{\"m\":\"{marker}\"}}")}),
        ),
        stop(2),
    ]);
    let mut reader = Reader::new(mapper.capture());
    let mut read = Ok(Progress::More);
    for event in &events {
        read = feed(&mut reader, &mut mapper, event);
    }
    let streaming = format!("{mapper:?}");
    end(&mut reader, &mut mapper, read);
    let reply = mapper.reply().expect("a whole reply");
    let debug = format!("{built:?} {refused:?} {conversation:?} {streaming} {reply:?}");
    let continued = next(
        conversation,
        TAG,
        &slug("claude-sonnet-5-5"),
        reply,
        &results(&["toolu_1"]),
    );
    let debug = format!("{debug} {continued:?}");
    assert!(continued.is_ok());
    assert!(!debug.contains(marker), "text leaked: {debug}");
    assert!(
        !debug.contains(&format!("{:?}", marker.as_bytes())),
        "bytes leaked: {debug}"
    );
}

/// A sealed synthetic JPEG with a marker in it, `len` bytes long.
fn sealed(len: usize) -> Jpeg {
    let mut bytes = vec![b'M'; len];
    bytes[..4].copy_from_slice(&[0xFF, 0xD8, 0xFF, 0xE0]);
    Jpeg::sealed(
        zeroize::Zeroizing::new(bytes),
        [8, 8],
        #[cfg(windows)]
        None,
    )
}

fn image_block(image: &Jpeg) -> Value {
    use base64::Engine as _;
    let (jpeg, _) = image.capture().expect("sealed");
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg",
        "data": base64::engine::general_purpose::STANDARD.encode(jpeg)}})
}

/// An authorized picture goes as exactly one image block, at its place among the user's blocks,
/// and a body or conversation's Debug says only that it has one.
#[test]
fn an_authorized_picture_goes_once_as_one_image_block_at_its_place() {
    let image = sealed(64);
    let block = image_block(&image);
    let text = |text: &str| json!({"type": "text", "text": text});
    let cases = [
        (
            vec![message(Role::User, "look"), InputItem::Image(image.clone())],
            json!([{"role": "user", "content": [text("look"), block]}]),
        ),
        (
            vec![
                message(Role::User, "look"),
                InputItem::Image(image.clone()),
                message(Role::Developer, "pointed"),
            ],
            json!([{"role": "user", "content": [text("look"), block, text("Kumochi note:\npointed")]}]),
        ),
        (
            vec![
                message(Role::User, "hi"),
                message(Role::Assistant, "yes"),
                InputItem::Image(image.clone()),
                message(Role::Developer, "pointed"),
            ],
            json!([
                {"role": "user", "content": [text("hi")]},
                {"role": "assistant", "content": [text("yes")]},
                {"role": "user", "content": [block, text("Kumochi note:\npointed")]},
            ]),
        ),
        (
            vec![
                message(Role::User, "hi"),
                message(Role::Assistant, "yes"),
                InputItem::Image(image),
            ],
            json!([
                {"role": "user", "content": [text("hi")]},
                {"role": "assistant", "content": [text("yes")]},
                {"role": "user", "content": [block]},
            ]),
        ),
    ];
    for (input, messages) in cases {
        let (sent, conversation) = body_under(&request(input), TAG, ON).expect("a body");
        let read: Value = serde_json::from_slice(sent.sent()).expect("json");
        assert_eq!(read["messages"], messages);
        let bytes = String::from_utf8_lossy(sent.sent()).into_owned();
        assert_eq!(bytes.matches(r#"{"type":"image""#).count(), 1);
        let shown = format!("{sent:?} {conversation:?}");
        assert!(
            !shown.contains("TU1N") && !shown.contains("image/jpeg"),
            "{shown}"
        );
    }
}

/// With the route's switch off, an unsealed image, a second image or one over the cap, no body
/// is built; a JPEG at the cap goes.
#[test]
fn a_picture_the_body_cant_carry_builds_nothing() {
    let input = |images: Vec<Jpeg>| {
        let mut input = vec![message(Role::User, "look")];
        input.extend(images.into_iter().map(InputItem::Image));
        request(input)
    };
    let unsealed = Jpeg::new(vec![0xFF, 0xD8, 0xFF, 0xE0]).expect("a JPEG's start");
    for (request, pictures) in [
        (input(vec![sealed(64)]), OFF),
        (input(vec![unsealed]), ON),
        (input(vec![sealed(64), sealed(64)]), ON),
        (input(vec![sealed(crate::screen::JPEG_CAP + 1)]), ON),
        (
            request(vec![
                message(Role::User, "look"),
                call("toolu_1", "list_tasks", "{}"),
                InputItem::Image(sealed(64)),
                output("toolu_1", "three tasks"),
            ]),
            ON,
        ),
    ] {
        assert_eq!(
            body_under(&request, TAG, pictures).err(),
            Some(BodyError::Image)
        );
    }
    use base64::Engine as _;

    let image = sealed(crate::screen::JPEG_CAP);
    let (jpeg, _) = image.capture().expect("sealed");
    let request = input(vec![image.clone()]);
    let (wire, _) = body_under(&request, TAG, ON).expect("JPEG at the limit");
    let body: Value = serde_json::from_slice(wire.sent()).expect("JSON body");
    let block = &body["messages"][0]["content"][1];
    assert_eq!(block["type"], "image");
    assert_eq!(block["source"]["type"], "base64");
    assert_eq!(block["source"]["media_type"], "image/jpeg");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(block["source"]["data"].as_str().expect("image data"))
        .expect("base64");
    assert!(
        decoded.as_slice() == jpeg,
        "the sent block preserves the ceiling JPEG"
    );
    assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 2);
}

/// A tool round keeps the picture where it went, once, and only while the switch is on.
#[test]
fn a_tool_round_keeps_its_picture_once_and_only_while_pictures_are_on() {
    let image = sealed(64);
    let pictured = request(vec![
        message(Role::User, "look"),
        InputItem::Image(image.clone()),
    ]);
    for pictures in [ON, OFF] {
        let (_, first) = body_under(&pictured, TAG, ON).expect("a body");
        let mut mapper = Mapper::answering(&first);
        streamed(&mut mapper, &called("toolu_1"));
        let reply = mapper.reply().expect("a whole reply");
        let model = slug("claude-sonnet-5-5");
        let next = next_under(first, TAG, &model, reply, &results(&["toolu_1"]), pictures);
        if pictures == OFF {
            assert_eq!(next.err(), Some(BodyError::Image));
            continue;
        }
        let (sent, _) = next.expect("continues");
        let read: Value = serde_json::from_slice(sent.sent()).expect("json");
        assert_eq!(read["messages"][0]["content"][1], image_block(&image));
        let bytes = String::from_utf8_lossy(sent.sent()).into_owned();
        assert_eq!(bytes.matches(r#"{"type":"image""#).count(), 1);
    }
}

/// Text that spells an image block stays an escaped string beside the one real block, and the
/// conversation a tool round keeps holds no copy of the picture: only the sent body has it.
#[test]
fn a_look_alike_stays_text_and_only_the_sent_body_holds_the_picture() {
    use base64::Engine as _;

    let image = sealed(64);
    let block = image_block(&image);
    let look_alike = json!({"type": "image", "source": {"type": "base64",
        "media_type": "image/jpeg", "data": "AAAA"}})
    .to_string();
    let input = vec![
        message(Role::User, &look_alike),
        call("toolu_1", "list_tasks", "{}"),
        output("toolu_1", &look_alike),
        message(Role::User, "look"),
        InputItem::Image(image.clone()),
    ];
    let (sent, conversation) = body_under(&request(input), TAG, ON).expect("a body");
    let read: Value = serde_json::from_slice(sent.sent()).expect("json");
    assert_eq!(
        read["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": look_alike}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "list_tasks", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": look_alike},
                {"type": "text", "text": "look"},
                block,
            ]},
        ])
    );
    let data = base64::engine::general_purpose::STANDARD.encode(image.capture().expect("sealed").0);
    let bytes = String::from_utf8_lossy(sent.sent()).into_owned();
    assert_eq!(bytes.matches(r#"{"type":"image""#).count(), 1);
    assert_eq!(bytes.matches(data.as_str()).count(), 1);
    let (unpictured, at) = conversation.wire().expect("wire");
    let kept = [
        String::from_utf8(unpictured).expect("UTF-8"),
        conversation.messages.concat(),
        serde_json::to_string(&conversation.head).expect("json"),
        format!("{sent:?} {conversation:?}"),
    ];
    assert!(at.is_some());
    for kept in kept {
        assert!(
            !kept.contains("TU1N") && !kept.contains(r#"{"type":"image""#),
            "{kept}"
        );
    }
}
