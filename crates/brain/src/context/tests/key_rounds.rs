//! The key's tool loop through the real Messages transport, reader, mapper, store and build,
//! offline: a signed round goes on from memory, and only its calls reach the store.

use std::fmt::Write;
use std::sync::{Arc, Mutex};

use kumochi_protocol::Secret;
use kumochi_protocol::message::ConnectionKind;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use super::*;
use crate::connections::anthropic::key::AnthropicKey;
use crate::connections::anthropic::tls::{Script, Scripted};
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::ModelClient;
use crate::coordinator::{BuildKey, Input, LeaseId, SendKey};
use crate::dispatch::ToolOutput;
use crate::engines::anthropic::encode::ToolUse;
use crate::engines::anthropic::live::{Live, Sending};
use crate::engines::{Answer, Asks, Finish, RunRequest, SendGate, SendLease, TurnEvent, answer};

const TAG: u64 = 41;
/// Round one's call as the store keeps it: its four fields, the input exactly as streamed.
const STORED: &str =
    r#"{"type":"tool_use","id":"toolu_1","name":"list_tasks","input":{"limit": 3}}"#;

fn on_key(tag: u64) -> RunSnapshot {
    let mut run = snapshot(Origin::Kumo, None);
    let choice = run.choice.as_mut().expect("a choice");
    choice.connection = ConnectionKind::AnthropicKey;
    choice.model = ModelSlug::new("claude-sonnet-5-5").expect("slug");
    run.connection = ConnectionTag(tag);
    run
}

/// A 200 response streaming `events`.
fn streamed(events: &[Value]) -> Vec<u8> {
    let mut body = String::new();
    for event in events {
        write!(body, "data: {event}\n\n").expect("a String takes any text");
    }
    let mut bytes = format!(
        "HTTP/1.1 200 Synthetic\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// A signed thinking block, text, and one call, then `tool_use`.
fn calling() -> Vec<u8> {
    streamed(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":5}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"THOUGHT-SENTINEL"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG-SENTINEL"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Looking."}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"list_tasks","input":{},"signature":"CALL-SIG-SENTINEL"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"limit\": 3}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
        json!({"type":"message_stop"}),
    ])
}

/// Two calls whose inputs a generic JSON parse would change: an object whose one member has
/// `serde_json`'s raw-value name, and a number past an `f64`.
const MARKED: &str = r#"{"$serde_json::private::RawValue":"5"}"#;
const PRECISE: &str = r#"{"n":123456789012345678901234567890.5}"#;

fn calling_two() -> Vec<u8> {
    let call = |index: u32, id: &str, input: &str| {
        [
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":"list_tasks","input":{}}}),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input}}),
            json!({"type":"content_block_stop","index":index}),
        ]
    };
    let mut events = vec![json!({"type":"message_start","message":{"usage":{"input_tokens":5}}})];
    events.extend(call(0, "toolu_1", MARKED));
    events.extend(call(1, "toolu_2", PRECISE));
    events.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}));
    events.push(json!({"type":"message_stop"}));
    streamed(&events)
}

/// A reply whose calls and stop reason disagree: a call that ends the turn, or `tool_use` with
/// no call.
fn disagreeing() -> [Vec<u8>; 2] {
    let start = json!({"type":"message_start","message":{"usage":{"input_tokens":5}}});
    let stop = |reason: &str| {
        [
            json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ]
    };
    let mut called = vec![
        start.clone(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"list_tasks","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    called.extend(stop("end_turn"));
    let mut said = vec![
        start,
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Hi."}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    said.extend(stop("tool_use"));
    [streamed(&called), streamed(&said)]
}

fn answering() -> Vec<u8> {
    streamed(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":5}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Done."}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ])
}

struct Peer {
    script: Arc<Mutex<Script>>,
    client: ModelClient<Scripted>,
    key: Secret<AnthropicKey>,
    budget: Budget,
}

impl Peer {
    fn new(responses: Vec<Vec<u8>>) -> Self {
        let script = Arc::new(Mutex::new(Script {
            responses: responses.into(),
            requests: Vec::new(),
        }));
        Self {
            client: ModelClient::new(Scripted(Arc::clone(&script))),
            script,
            key: AnthropicKey::parse(Secret::new(Zeroizing::new(
                b"sk-ant-api-fake-only".to_vec(),
            )))
            .expect("synthetic key"),
            budget: Budget::limited(None),
        }
    }

    /// Plays `request`'s round on `live`, answering its ask with `answer_with` a lease, and
    /// returns its ending and every event it sent.
    async fn play(
        &self,
        live: &Live,
        request: &RunRequest,
        answer_with: fn(SendLease) -> Answer,
    ) -> (Option<Finish>, Vec<TurnEvent>) {
        let (inputs_tx, mut inputs) = mpsc::unbounded_channel();
        let asks = Asks::default();
        let gate = SendGate::new(request.key, Arc::clone(&asks), inputs_tx.clone());
        let (events_tx, mut events) = mpsc::channel(64);
        let sending = Sending {
            key: &self.key,
            tag: ConnectionTag(TAG),
            run: &self.budget,
            live: &self.budget,
        };
        let finish = {
            let round = live.round(&self.client, request, sending, &events_tx, &gate);
            tokio::pin!(round);
            loop {
                tokio::select! {
                    finish = &mut round => break finish,
                    Some(Input::SendReady { key, .. }) = inputs.recv() => {
                        let lease = SendLease::new(LeaseId { key, send: 1 }, inputs_tx.clone());
                        answer(&asks, key, answer_with(lease));
                    }
                }
            }
        };
        drop(events_tx);
        let mut sent = Vec::new();
        while let Some(event) = events.recv().await {
            sent.push(event);
        }
        (finish, sent)
    }

    /// The JSON body of each request sent so far.
    fn bodies(&self) -> Vec<Value> {
        self.sent()
            .iter()
            .map(|body| serde_json::from_str(body).expect("a JSON body"))
            .collect()
    }

    /// The body of each request sent so far, as sent.
    fn sent(&self) -> Vec<String> {
        let script = self.script.lock().expect("script");
        script
            .requests
            .iter()
            .map(|request| {
                let head = request
                    .windows(4)
                    .position(|bytes| bytes == b"\r\n\r\n")
                    .expect("an HTTP head");
                String::from_utf8(request[head + 4..].to_vec()).expect("a UTF-8 body")
            })
            .collect()
    }
}

fn request(world: &World, run: RunId, round: u32) -> RunRequest {
    let built = world
        .build(&on_key(TAG), (world.main, run), Supplied::default(), BOUND)
        .expect("built");
    RunRequest {
        key: SendKey::Build(BuildKey {
            run,
            round,
            attempt: u64::from(round),
        }),
        run,
        thread: world.main,
        origin: Origin::Kumo,
        request: built.request,
    }
}

fn stored_item() -> Opaque {
    Opaque::new(STORED.as_bytes().to_vec()).expect("small")
}

fn stored_call() -> ToolUse {
    ToolUse::stored(STORED.as_bytes()).expect("a call")
}

/// The calls a round handed on, at their places in the reply.
fn key_calls(events: &[TurnEvent]) -> Vec<(u32, ToolUse)> {
    events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::KeyCall { index, call } => Some((*index, call.clone())),
            _ => None,
        })
        .collect()
}

/// Stores a round as the coordinator does: its text, `calls`, and `results` in the order given.
fn keep_calls(world: &mut World, run: RunId, calls: &[ToolUse], results: &[(&str, &str)]) {
    let results: Vec<_> = results
        .iter()
        .map(|(id, output)| (call(id), ToolOutput::Text((*output).into())))
        .collect();
    world
        .store
        .append_key_round(
            (world.main, run),
            ConnectionTag(TAG),
            Some("Looking."),
            calls,
            &results,
        )
        .expect("stored");
}

/// Stores round one: its text, its call, and the call's result.
fn keep_round(world: &mut World, run: RunId) {
    keep_calls(world, run, &[stored_call()], &[("toolu_1", "three tasks")]);
}

/// Every stored event's text and body.
fn stored(world: &World) -> String {
    let conn = world.store.conn_for_tests();
    let mut statement = conn
        .prepare("SELECT COALESCE(text, '') || COALESCE(body, '') FROM events")
        .expect("prepared");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("rows")
        .map(|row| row.expect("row"))
        .collect()
}

#[tokio::test]
async fn a_signed_key_round_goes_on_from_memory_and_only_its_call_is_stored() {
    let mut world = world("key-round-memory");
    let run = world.start("ask");
    let peer = Peer::new(vec![calling(), answering()]);
    let live = Live::default();

    let (finish, events) = peer
        .play(&live, &request(&world, run, 1), Answer::Grant)
        .await;
    assert_eq!(finish, Some(Finish::Completed));
    let calls = key_calls(&events);
    let handed: Vec<(u32, String)> = calls.iter().map(|(at, call)| (*at, call.json())).collect();
    assert_eq!(handed, [(2, STORED.to_owned())]);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, TurnEvent::OutputItem { .. }))
    );
    assert!(events.contains(&TurnEvent::TextDelta("Looking.".into())));

    let before = stored(&world);
    for item in [
        r#"{"type":"thinking","thinking":"t","signature":"s"}"#,
        r#"{"type":"tool_use","id":"toolu_1","name":"list_tasks","input":{},"signature":"s"}"#,
    ] {
        let refused = world.store.append_round(
            (world.main, run),
            Route {
                kind: ConnectionKind::AnthropicKey,
                tag: ConnectionTag(TAG),
            },
            Some("Looking."),
            &[Opaque::new(item.as_bytes().to_vec()).expect("small")],
            &[],
        );
        assert!(
            matches!(refused, Err(StoreError::RoundRecord)),
            "the key's rounds store only typed calls: {refused:?}"
        );
        assert_eq!(ToolUse::stored(item.as_bytes()), None, "{item}");
    }
    assert_eq!(stored(&world), before, "a refused round writes nothing");
    let calls: Vec<ToolUse> = calls.into_iter().map(|(_, call)| call).collect();
    keep_calls(&mut world, run, &calls, &[("toolu_1", "three tasks")]);
    let rows = stored(&world);
    assert!(rows.contains("toolu_1"), "{rows}");
    assert!(
        !rows.contains("SENTINEL") && !rows.contains("signature"),
        "{rows}"
    );

    let (finish, events) = peer
        .play(&live, &request(&world, run, 2), Answer::Grant)
        .await;
    assert_eq!(finish, Some(Finish::Completed));
    assert!(events.contains(&TurnEvent::TextDelta("Done.".into())));
    let bodies = peer.bodies();
    let [first, second] = bodies.as_slice() else {
        panic!("two requests, not {}", bodies.len());
    };
    let sent = first["messages"].as_array().expect("messages");
    let next = second["messages"].as_array().expect("messages");
    assert_eq!(
        next[..sent.len()],
        sent[..],
        "the first request goes again unchanged"
    );
    assert_eq!(second["system"], first["system"]);
    assert_eq!(second["tools"], first["tools"]);
    assert_eq!(
        next[sent.len()..],
        [
            json!({"role":"assistant","content":[
                {"type":"thinking","thinking":"THOUGHT-SENTINEL","signature":"SIG-SENTINEL"},
                {"type":"text","text":"Looking."},
                {"type":"tool_use","id":"toolu_1","name":"list_tasks","input":{"limit":3},"signature":"CALL-SIG-SENTINEL"},
            ]}),
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":"three tasks"},
            ]}),
        ]
    );
}

#[tokio::test]
async fn a_later_round_without_its_continuation_is_refused_unsent() {
    let mut world = world("key-round-refused");
    let run = world.start("ask");
    let peer = Peer::new(vec![calling(), answering()]);
    let live = Live::default();
    keep_round(&mut world, run);
    let refused = Some(Finish::Failed(crate::engines::EngineFailure::Anthropic(
        crate::connections::anthropic::error::Failure::Request,
    )));
    assert_eq!(
        peer.play(&live, &request(&world, run, 2), Answer::Grant)
            .await,
        (refused.clone(), Vec::new()),
        "a stored round is never continued"
    );

    let mut world = super::world("key-round-rebuilt");
    let run = world.start("ask");
    let fresh = Live::default();
    let first = request(&world, run, 1);
    let (finish, _) = peer.play(&fresh, &first, Answer::Grant).await;
    assert_eq!(finish, Some(Finish::Completed));
    keep_round(&mut world, run);
    let second = request(&world, run, 2);
    assert_eq!(
        peer.play(&fresh, &second, |_| Answer::Rebuild).await,
        (None, Vec::new())
    );
    assert_eq!(
        peer.play(&fresh, &second, Answer::Grant).await,
        (refused, Vec::new()),
        "a continuation a rebuild dropped is never rebuilt"
    );
    assert_eq!(peer.bodies().len(), 1);
}

#[test]
fn a_key_round_replays_natively_only_on_its_kind_and_tag() {
    let mut world = world("key-round-replay");
    let first = world.start("ask");
    keep_round(&mut world, first);
    world.reply(first, "Done.");
    let next = world.start("next");
    for (run, native) in [
        (on_key(TAG), true),
        (on_key(TAG + 1), false),
        (
            {
                let mut run = on_key(TAG);
                run.choice.as_mut().expect("a choice").connection = ConnectionKind::ChatGpt;
                run
            },
            false,
        ),
    ] {
        let built = world
            .build(&run, (world.main, next), Supplied::default(), BOUND)
            .expect("built");
        // Compared as JSON: the store keeps an item's value, not always its key order.
        let read = |items: &[InputItem]| -> Vec<InputItem> {
            items
                .iter()
                .map(|item| match item {
                    InputItem::Native(item) => {
                        let value: Value = serde_json::from_slice(item.bytes()).expect("JSON");
                        InputItem::Native(
                            Opaque::new(value.to_string().into_bytes()).expect("small"),
                        )
                    }
                    item => item.clone(),
                })
                .collect()
        };
        let input = &built.request.input;
        let said = |text: &str| InputItem::Message {
            role: Role::Assistant,
            text: text.into(),
        };
        let expected: Vec<InputItem> = if native {
            vec![
                said("Looking."),
                InputItem::Native(stored_item()),
                InputItem::FunctionOutput {
                    call: call("toolu_1"),
                    output: "three tasks".into(),
                },
                said("Done."),
            ]
        } else {
            vec![said("Looking.Done.")]
        };
        assert_eq!(read(&input[1..input.len() - 1]), read(&expected), "{run:?}");
    }
}

#[tokio::test]
async fn a_key_calls_input_keeps_its_bytes_through_the_store_and_later_requests() {
    let mut world = world("key-round-bytes");
    let run = world.start("ask");
    let peer = Peer::new(vec![calling_two(), answering(), answering()]);
    let live = Live::default();
    let (finish, events) = peer
        .play(&live, &request(&world, run, 1), Answer::Grant)
        .await;
    assert_eq!(finish, Some(Finish::Completed));
    let calls: Vec<ToolUse> = key_calls(&events)
        .into_iter()
        .map(|(_, call)| call)
        .collect();
    let expected = [
        format!(r#"{{"type":"tool_use","id":"toolu_1","name":"list_tasks","input":{MARKED}}}"#),
        format!(r#"{{"type":"tool_use","id":"toolu_2","name":"list_tasks","input":{PRECISE}}}"#),
    ];
    assert_eq!(
        calls.iter().map(ToolUse::json).collect::<Vec<_>>(),
        expected
    );
    // Stored with its results the other way round.
    keep_calls(
        &mut world,
        run,
        &calls,
        &[("toolu_2", "two"), ("toolu_1", "one")],
    );
    let rows = stored(&world);
    assert!(
        expected.iter().all(|call| rows.contains(call.as_str())),
        "{rows}"
    );

    let (finish, _) = peer
        .play(&live, &request(&world, run, 2), Answer::Grant)
        .await;
    assert_eq!(finish, Some(Finish::Completed));
    let bodies = peer.bodies();
    let results = bodies[1]["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .expect("a last message");
    assert_eq!(
        *results,
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":"one"},
            {"type":"tool_result","tool_use_id":"toolu_2","content":"two"},
        ]}),
        "each call gets its own result, in call order"
    );

    world.reply(run, "Done.");
    let next = world.start("next");
    let (finish, _) = peer
        .play(&live, &request(&world, next, 1), Answer::Grant)
        .await;
    assert_eq!(finish, Some(Finish::Completed));
    let later = &peer.sent()[2];
    assert!(
        expected.iter().all(|call| later.contains(call.as_str())),
        "a later request sends each stored call as read: {later}"
    );
}

#[tokio::test]
async fn a_reply_whose_calls_and_ending_disagree_fails_and_holds_nothing() {
    let protocol = Some(Finish::Failed(crate::engines::EngineFailure::Anthropic(
        crate::connections::anthropic::error::Failure::Protocol,
    )));
    let [called, said] = disagreeing();
    let mut world = world("key-round-disagree");
    let run = world.start("ask");
    let peer = Peer::new(vec![called, said, answering()]);
    let live = Live::default();
    let (finish, events) = peer
        .play(&live, &request(&world, run, 1), Answer::Grant)
        .await;
    assert_eq!(finish, protocol, "a call that ends the turn");
    assert_eq!(key_calls(&events), Vec::new());
    keep_round(&mut world, run);
    let refused = Some(Finish::Failed(crate::engines::EngineFailure::Anthropic(
        crate::connections::anthropic::error::Failure::Request,
    )));
    assert_eq!(
        peer.play(&live, &request(&world, run, 2), Answer::Grant)
            .await
            .0,
        refused,
        "nothing was held to continue"
    );
    let other = world.start("again");
    let (finish, events) = peer
        .play(&live, &request(&world, other, 1), Answer::Grant)
        .await;
    assert_eq!(finish, protocol, "tool_use with no call");
    assert_eq!(key_calls(&events), Vec::new());
    assert_eq!(peer.bodies().len(), 2);
}

#[test]
fn a_key_run_with_another_routes_row_or_unpaired_results_replays_as_its_text() {
    use crate::recall::Query;
    let mut world = world("key-round-provenance");
    let (_task, _) = world.run(None, "needle B-private-sentinel", None);
    let main = world.main;
    let first = world.start("ask");
    keep_round(&mut world, first);
    let selected = Snapshot::begin(world.store.conn_for_tests())
        .expect("snapshot")
        .select_recall(
            Origin::Kumo,
            &Query::Search {
                query: "needle".into(),
                task: None,
            },
        )
        .expect("selected");
    world
        .store
        .append_round(
            (main, first),
            Route::chatgpt(TAG),
            None,
            &[Opaque::new(
                br#"{"type":"function_call","call_id":"recall","name":"search_history","arguments":"{}"}"#
                    .to_vec(),
            )
            .expect("small")],
            &[(call("recall"), ToolOutput::Recall(selected))],
        )
        .expect("a foreign round");
    world.reply(first, "Done.");
    let doubled = world.start("again");
    keep_calls(
        &mut world,
        doubled,
        &[stored_call()],
        &[("toolu_1", "one"), ("toolu_1", "two")],
    );
    world.reply(doubled, "Done.");
    let unanswered = world.start("alone");
    keep_calls(&mut world, unanswered, &[stored_call()], &[]);
    world.reply(unanswered, "Done.");
    let next = world.start("next");
    let mut chatgpt = on_key(TAG);
    chatgpt.choice.as_mut().expect("a choice").connection = ConnectionKind::ChatGpt;
    for run in [on_key(TAG), chatgpt] {
        let built = world
            .build(&run, (main, next), Supplied::default(), BOUND)
            .expect("built");
        let said = |text: &str| InputItem::Message {
            role: Role::Assistant,
            text: text.into(),
        };
        let input = &built.request.input;
        assert_eq!(
            input[1..input.len() - 1],
            [
                said("Looking.Done."),
                InputItem::Message {
                    role: Role::User,
                    text: "again".into(),
                },
                said("Looking.Done."),
                InputItem::Message {
                    role: Role::User,
                    text: "alone".into(),
                },
                said("Looking.Done."),
            ],
            "{run:?}"
        );
        assert_eq!(built.sources, vec![main], "no recall was read: {run:?}");
        assert!(!format!("{input:?}").contains("private-sentinel"));
    }
}
