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
use crate::engines::anthropic::live::{Live, Sending};
use crate::engines::{Answer, Asks, Finish, RunRequest, SendGate, SendLease, TurnEvent, answer};

const TAG: u64 = 41;
const STORED: &str =
    r#"{"type":"tool_use","id":"toolu_1","name":"list_tasks","input":{"limit":3}}"#;

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
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"list_tasks","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"limit\": 3}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}),
        json!({"type":"message_stop"}),
    ])
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
        let script = self.script.lock().expect("script");
        script
            .requests
            .iter()
            .map(|request| {
                let head = request
                    .windows(4)
                    .position(|bytes| bytes == b"\r\n\r\n")
                    .expect("an HTTP head");
                serde_json::from_slice(&request[head + 4..]).expect("a JSON body")
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

/// Stores round one as the coordinator does: its text, its call, and the call's result.
fn keep_round(world: &mut World, run: RunId) {
    world
        .store
        .append_round(
            (world.main, run),
            Route {
                kind: ConnectionKind::AnthropicKey,
                tag: ConnectionTag(TAG),
            },
            Some("Looking."),
            &[stored_item()],
            &[(call("toolu_1"), ToolOutput::Text("three tasks".into()))],
        )
        .expect("stored");
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
    let items: Vec<(u32, Value)> = events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::OutputItem { index, item } => {
                Some((*index, serde_json::from_slice(item.bytes()).expect("JSON")))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        items,
        [(2, serde_json::from_str::<Value>(STORED).expect("JSON"))]
    );
    assert!(events.contains(&TurnEvent::TextDelta("Looking.".into())));

    keep_round(&mut world, run);
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
                {"type":"tool_use","id":"toolu_1","name":"list_tasks","input":{"limit":3}},
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
