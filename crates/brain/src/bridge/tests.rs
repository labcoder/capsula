use std::io::{Cursor, Write};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use kumochi_protocol::frame::{self, Pipe};
use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt as _, DuplexStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::id::{JsonRpcId, MAX_STRING_ID};
use super::stdio::{Body, Incoming, Line, LineReader, RpcError, classify, reply};
use super::wire::{CHUNK, CallKey, Method, Reassembly, ToBrain, ToBridge, TooLarge, chunks};
use super::{BridgeError, Input, MAX_INCOMING, MAX_REPLY, connect, relay, run};
use crate::platform::{Instance, PipeServer, create_owner_only, test_pair};

#[test]
fn ids_are_numbers_and_short_strings_and_their_keys_never_collide() {
    let id = |value: Value| JsonRpcId::from_value(&value);
    assert_eq!(id(json!(1)), Some(JsonRpcId::Number(1)));
    assert_eq!(id(json!(-1)), Some(JsonRpcId::Number(-1)));
    assert_eq!(id(json!(i64::MAX)), Some(JsonRpcId::Number(i64::MAX)));
    assert_eq!(id(json!(u64::MAX)), None);
    assert_eq!(id(json!(1.5)), None);
    assert_eq!(id(json!(null)), None);
    assert_eq!(id(json!({"a": 1})), None);
    assert_eq!(id(json!(true)), None);
    let longest = "a".repeat(MAX_STRING_ID);
    assert_eq!(id(json!(longest)), Some(JsonRpcId::String(longest.clone())));
    assert_eq!(id(json!("a".repeat(MAX_STRING_ID + 1))), None);
    assert_eq!(JsonRpcId::Number(1).key(), CallKey("m:n:1".to_owned()));
    assert_eq!(
        JsonRpcId::String("1".to_owned()).key(),
        CallKey("m:s:31".to_owned())
    );
    assert_eq!(JsonRpcId::String(longest).key().0.len(), 62);
}

#[test]
fn lines_are_classified_by_envelope_method_and_id() {
    let request = |line: &str| classify(line.as_bytes());
    assert_eq!(request("  "), Incoming::Blank);
    assert_eq!(
        request(r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"noop"}}"#),
        Incoming::Request {
            id: JsonRpcId::Number(7),
            method: Method::ToolsCall,
            params: br#"{"name":"noop"}"#.to_vec()
        }
    );
    assert_eq!(
        request(r#"{"jsonrpc":"2.0","id":"\u0041","method":"tools/list"}"#),
        Incoming::Request {
            id: JsonRpcId::String("A".to_owned()),
            method: Method::ToolsList,
            params: b"null".to_vec()
        }
    );
    assert_eq!(
        request(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        Incoming::Notification { initialized: true }
    );
    assert_eq!(
        request(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#),
        Incoming::Notification { initialized: false }
    );
    let invalid = |id, error| Incoming::Invalid { id, error };
    for (line, expected) in [
        (
            r#"{"jsonrpc":"2.0","id":1,"method":"res"#,
            invalid(None, RpcError::Parse),
        ),
        (
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
            invalid(None, RpcError::InvalidRequest),
        ),
        ("3", invalid(None, RpcError::InvalidRequest)),
        (
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            invalid(None, RpcError::InvalidRequest),
        ),
        (
            r#"{"jsonrpc":"2.0","id":{},"method":"ping"}"#,
            invalid(None, RpcError::InvalidRequest),
        ),
        (
            r#"{"id":2,"method":"ping"}"#,
            invalid(Some(JsonRpcId::Number(2)), RpcError::InvalidRequest),
        ),
        (
            r#"{"jsonrpc":"2.0","id":2}"#,
            invalid(Some(JsonRpcId::Number(2)), RpcError::InvalidRequest),
        ),
        (
            r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#,
            invalid(Some(JsonRpcId::Number(2)), RpcError::MethodNotFound),
        ),
    ] {
        assert_eq!(request(line), expected, "{line}");
    }
}

#[test]
fn replies_carry_the_id_by_value_and_the_body_unchanged() {
    let text = |line: Vec<u8>| String::from_utf8(line).expect("utf-8");
    assert_eq!(
        text(reply(
            Some(&JsonRpcId::Number(7)),
            Body::Result(br#"{"content":[]}"#)
        )),
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"content\":[]}}\n"
    );
    assert_eq!(
        text(reply(
            Some(&JsonRpcId::String("A\"".to_owned())),
            Body::Error(br#"{"code":1}"#)
        )),
        "{\"jsonrpc\":\"2.0\",\"id\":\"A\\\"\",\"error\":{\"code\":1}}\n"
    );
    assert_eq!(
        text(reply(None, Body::Local(RpcError::InvalidRequest))),
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32600,\"message\":\"Invalid Request\"}}\n"
    );
}

fn lines(chunks: &[&[u8]]) -> (Vec<Line>, Option<Line>) {
    let mut reader = LineReader::default();
    let mut out = Vec::new();
    for chunk in chunks {
        reader.push(chunk, &mut out);
    }
    let last = reader.finish();
    (out, last)
}

#[test]
fn a_line_of_exactly_the_cap_passes_and_one_over_is_refused_then_dropped() {
    let exact = vec![b'a'; MAX_INCOMING];
    let (out, last) = lines(&[&exact, b"\n"]);
    assert_eq!(out, vec![Line::Text(exact)]);
    assert_eq!(last, None);

    let over = vec![b'a'; MAX_INCOMING + 1];
    let (out, last) = lines(&[&over[..10], &over[10..], b"tail\nnext\r\n", b"cut"]);
    assert_eq!(
        out,
        vec![Line::TooLong, Line::Text(b"next".to_vec())],
        "the over-long line's rest is dropped and the next one kept"
    );
    assert_eq!(
        last,
        Some(Line::Text(b"cut".to_vec())),
        "a last line with no newline"
    );

    let (out, last) = lines(&[b"\n", b"a", b"b\r", b"\n"]);
    assert_eq!(out, vec![Line::Text(vec![]), Line::Text(b"ab".to_vec())]);
    assert_eq!(last, None);
}

#[test]
fn bodies_are_chunked_under_the_frame_cap_and_reassembled_up_to_their_cap() {
    let body = vec![7; MAX_INCOMING];
    let parts: Vec<(usize, bool)> = chunks(&body)
        .map(|(chunk, last)| (chunk.len(), last))
        .collect();
    assert_eq!(
        parts,
        [
            (CHUNK, false),
            (CHUNK, false),
            (MAX_INCOMING - 2 * CHUNK, true)
        ]
    );
    assert_eq!(
        chunks(&[])
            .map(|(chunk, last)| (chunk.len(), last))
            .collect::<Vec<_>>(),
        [(0, true)]
    );
    for (chunk, _) in chunks(&body) {
        let frame = frame::encode(
            &ToBrain::Request {
                key: JsonRpcId::String("a".repeat(MAX_STRING_ID)).key(),
                method: Method::ToolsCall,
                chunk: chunk.to_vec(),
                last: false,
            },
            Pipe::Data,
        );
        assert!(frame.is_ok(), "a full chunk fits a data frame");
    }

    let mut exact = Reassembly::new(MAX_INCOMING);
    chunks(&body).for_each(|(chunk, _)| exact.push(chunk));
    assert_eq!(exact.finish(), Ok(body));
    let over = vec![7; MAX_INCOMING + 1];
    let mut refused = Reassembly::new(MAX_INCOMING);
    chunks(&over).for_each(|(chunk, _)| refused.push(chunk));
    assert_eq!(refused.finish(), Err(TooLarge));
}

/// The bridge's relay on one end of a real pipe, and the brain's end for the test to drive.
struct Bridge {
    stdin: mpsc::Sender<Input>,
    stdout: mpsc::Receiver<Vec<u8>>,
    brain: PipeServer,
    relay: JoinHandle<Result<(), BridgeError>>,
}

/// A bridge whose brain has said `Hello` with `remaining_ms` left.
async fn bridge(name: &str, remaining_ms: u64) -> Bridge {
    let (mut brain, client) = test_pair(name).await.expect("pipe pair");
    hello(&mut brain, remaining_ms).await;
    let (stdin, input) = mpsc::channel(16);
    let (out, stdout) = mpsc::channel(4);
    let relay = tokio::spawn(relay(client, input, out));
    Bridge {
        stdin,
        stdout,
        brain,
        relay,
    }
}

async fn hello(brain: &mut (impl AsyncWrite + Unpin), remaining_ms: u64) {
    let frame = frame::encode(&ToBridge::Hello { remaining_ms }, Pipe::Data).expect("encodes");
    brain.write_all(&frame).await.expect("write");
}

impl Bridge {
    async fn line(&self, text: &str) {
        self.stdin
            .send(Input::Line(Line::Text(text.as_bytes().to_vec())))
            .await
            .expect("relay running");
    }

    async fn stdout(&mut self) -> String {
        let line = tokio::time::timeout(Duration::from_secs(5), self.stdout.recv()).await;
        String::from_utf8(line.expect("a stdout line in time").expect("relay running"))
            .expect("utf-8")
    }

    async fn request(&mut self) -> (CallKey, Method, Vec<u8>) {
        let mut params = Vec::new();
        loop {
            let message = crate::ipc::read(&mut self.brain, Pipe::Data)
                .await
                .expect("frame");
            match message {
                Some(ToBrain::Request {
                    key,
                    method,
                    chunk,
                    last,
                }) => {
                    params.extend(chunk);
                    if last {
                        return (key, method, params);
                    }
                }
                other => panic!("{other:?}"),
            }
        }
    }

    async fn send(&mut self, message: &ToBridge) {
        let frame = frame::encode(message, Pipe::Data).expect("encodes");
        self.brain.write_all(&frame).await.expect("write");
    }

    async fn answer(&mut self, key: &CallKey, body: Body<'_>) {
        let (body, error) = match body {
            Body::Result(body) => (body, false),
            Body::Error(body) => (body, true),
            Body::Local(_) => panic!("the brain sends bodies only"),
        };
        for (chunk, last) in chunks(body) {
            self.send(&ToBridge::Reply {
                key: key.clone(),
                chunk: chunk.to_vec(),
                last,
                error,
            })
            .await;
        }
    }
}

const CALL: &str =
    r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"noop","arguments":{}}}"#;

#[tokio::test]
async fn a_call_is_relayed_and_answered_with_its_id() {
    let mut bridge = bridge("bridge-round-trip", 60_000).await;
    bridge
        .line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
        .await;
    let initialized = crate::ipc::read::<ToBrain>(&mut bridge.brain, Pipe::Data)
        .await
        .expect("frame");
    assert!(
        matches!(initialized, Some(ToBrain::Initialized)),
        "{initialized:?}"
    );
    bridge.line(CALL).await;
    let (key, method, params) = bridge.request().await;
    assert_eq!((key.0.as_str(), method), ("m:n:7", Method::ToolsCall));
    assert_eq!(
        serde_json::from_slice::<Value>(&params).expect("json"),
        json!({"name": "noop", "arguments": {}})
    );
    bridge
        .answer(&key, Body::Result(br#"{"content":[]}"#))
        .await;
    assert_eq!(
        bridge.stdout().await,
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"content\":[]}}\n"
    );
    bridge.line(CALL).await;
    let (key, ..) = bridge.request().await;
    bridge
        .answer(&key, Body::Error(br#"{"code":-1,"message":"no"}"#))
        .await;
    assert_eq!(
        bridge.stdout().await,
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"error\":{\"code\":-1,\"message\":\"no\"}}\n"
    );
    drop(bridge.stdin);
    assert!(
        bridge.relay.await.expect("joins").is_ok(),
        "stdin's end ends the bridge"
    );
}

#[tokio::test]
async fn one_call_at_a_time_and_a_ping_is_answered_meanwhile() {
    let mut bridge = bridge("bridge-busy", 60_000).await;
    bridge.line(CALL).await;
    let (key, ..) = bridge.request().await;
    bridge
        .line(r#"{"jsonrpc":"2.0","id":"b","method":"tools/call","params":{}}"#)
        .await;
    assert_eq!(
        bridge.stdout().await,
        "{\"jsonrpc\":\"2.0\",\"id\":\"b\",\"error\":{\"code\":-32600,\"message\":\"Invalid Request\"}}\n"
    );
    bridge
        .line(r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#)
        .await;
    assert_eq!(
        bridge.stdout().await,
        "{\"jsonrpc\":\"2.0\",\"id\":\"p\",\"result\":{}}\n"
    );
    bridge
        .answer(&CallKey("m:n:99".to_owned()), Body::Result(b"{}"))
        .await;
    bridge.answer(&key, Body::Result(b"{}")).await;
    assert_eq!(
        bridge.stdout().await,
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n",
        "another key's reply is dropped"
    );
}

#[tokio::test]
async fn a_reply_of_exactly_the_cap_passes_and_one_over_is_an_internal_error() {
    let mut bridge = bridge("bridge-reply-cap", 60_000).await;
    for (len, ok) in [(MAX_REPLY, true), (MAX_REPLY + 1, false)] {
        let body = format!("\"{}\"", "a".repeat(len - 2));
        bridge.line(CALL).await;
        let (key, ..) = bridge.request().await;
        bridge.answer(&key, Body::Result(body.as_bytes())).await;
        let line = bridge.stdout().await;
        if ok {
            assert_eq!(
                line.len(),
                len + "{\"jsonrpc\":\"2.0\",\"id\":7,\"result\":}\n".len()
            );
        } else {
            assert_eq!(
                line,
                "{\"jsonrpc\":\"2.0\",\"id\":7,\"error\":{\"code\":-32603,\"message\":\"Internal error\"}}\n"
            );
        }
    }
}

#[tokio::test]
async fn a_lost_pipe_ends_the_bridge_without_an_error() {
    let mut bridge = bridge("bridge-lost", 60_000).await;
    bridge.line(CALL).await;
    bridge.request().await;
    drop(bridge.brain);
    assert!(bridge.relay.await.expect("joins").is_ok());
}

#[tokio::test]
async fn the_deadline_fails_the_call_in_flight_and_ends_the_bridge() {
    let mut bridge = bridge("bridge-deadline", 300).await;
    bridge.line(CALL).await;
    bridge.request().await;
    assert_eq!(
        bridge.stdout().await,
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"error\":{\"code\":-32603,\"message\":\"Internal error\"}}\n"
    );
    assert!(matches!(
        bridge.relay.await.expect("joins"),
        Err(BridgeError::Deadline)
    ));
}

#[tokio::test]
async fn a_pipe_served_by_another_executable_is_refused() {
    let name = format!(
        r"\\.\pipe\{}-test-{}-bridge-server",
        kumochi_protocol::constants::OBJECT_PREFIX,
        std::process::id()
    );
    let _server = create_owner_only(&name, Instance::First).expect("server");
    assert!(matches!(
        connect(&name, std::path::Path::new(r"C:\elsewhere\kumochid.exe")),
        Err(BridgeError::NotOurBrain)
    ));
    let _second = create_owner_only(&name, Instance::Next).expect("second instance");
    let own = std::env::current_exe().expect("test executable");
    assert!(connect(&name, &own).is_ok(), "this process serves it");
}

#[test]
fn debug_prints_no_id_line_params_or_body() {
    const PLANTED: &str = "kumochi-planted";
    // "kumo" as a byte list and as the hex a call key spells.
    let (decimal, hex) = ("107, 117, 109, 111", "6b756d6f");
    let id = JsonRpcId::String(PLANTED.to_owned());
    let line = format!(
        r#"{{"jsonrpc":"2.0","id":"{PLANTED}","method":"tools/call","params":{{"text":"{PLANTED}"}}}}"#
    );
    let printed = [
        format!("{:?}", Input::Line(Line::Text(line.clone().into_bytes()))),
        format!("{:?}", classify(line.as_bytes())),
        format!(
            "{:?}",
            classify(br#"{"jsonrpc":"2.0","id":424242,"method":"nope"}"#)
        ),
        format!(
            "{:?}",
            ToBrain::Request {
                key: id.key(),
                method: Method::ToolsCall,
                chunk: line.into_bytes(),
                last: true
            }
        ),
        format!(
            "{:?}",
            ToBridge::Reply {
                key: id.key(),
                chunk: PLANTED.as_bytes().to_vec(),
                last: true,
                error: false
            }
        ),
        format!("{:?}", Some(id)),
    ];
    for text in printed {
        for needle in [PLANTED, decimal, hex, "424242"] {
            assert!(!text.contains(needle), "{needle} in {text}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn no_hello_in_time_or_another_message_first_ends_the_bridge() {
    let (_brain, client) = tokio::io::duplex(1024);
    let (_stdin, input) = mpsc::channel(16);
    let (out, _stdout) = mpsc::channel(4);
    assert!(matches!(
        relay(client, input, out).await,
        Err(BridgeError::NoHello)
    ));

    let (mut brain, client) = tokio::io::duplex(1024);
    let reply = ToBridge::Reply {
        key: CallKey("m:n:1".to_owned()),
        chunk: vec![],
        last: true,
        error: false,
    };
    brain
        .write_all(&frame::encode(&reply, Pipe::Data).expect("encodes"))
        .await
        .expect("write");
    let (_stdin, input) = mpsc::channel(16);
    let (out, _stdout) = mpsc::channel(4);
    assert!(matches!(
        relay(client, input, out).await,
        Err(BridgeError::NoHello)
    ));
}

#[tokio::test]
async fn a_brain_that_stops_reading_cannot_hold_a_request_past_the_deadline() {
    let (mut brain, client) = tokio::io::duplex(1024);
    hello(&mut brain, 200).await;
    let (stdin, input) = mpsc::channel(16);
    let (out, mut stdout) = mpsc::channel(4);
    let relay = tokio::spawn(relay(client, input, out));
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"text":"{}"}}}}"#,
        "a".repeat(CHUNK + 1)
    );
    stdin
        .send(Input::Line(Line::Text(call.into_bytes())))
        .await
        .expect("relay running");
    let ended = tokio::time::timeout(Duration::from_secs(5), relay).await;
    assert!(matches!(
        ended.expect("ends in time").expect("joins"),
        Err(BridgeError::Deadline)
    ));
    assert_eq!(
        String::from_utf8(stdout.recv().await.expect("the call's failure")).expect("utf-8"),
        "{\"jsonrpc\":\"2.0\",\"id\":7,\"error\":{\"code\":-32603,\"message\":\"Internal error\"}}\n"
    );
    drop(brain);
}

/// A stdout nobody reads: every write says it started, then waits until the test ends.
struct Withheld(std_mpsc::Receiver<()>, std_mpsc::Sender<()>);

impl Write for Withheld {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        self.1.send(()).unwrap_or_default();
        self.0.recv().unwrap_or_default();
        Err(std::io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What [`withheld`] does with the brain's end.
#[derive(Clone, Copy)]
enum BrainEnd {
    Kept,
    /// Dropped once a line is stuck in stdout.
    LostOnceStuck,
}

/// Runs the whole bridge on its own thread against a brain that said `Hello`, with a stdout
/// nobody reads and twenty pings on stdin.
fn withheld(remaining_ms: u64, end: BrainEnd) -> (Result<(), BridgeError>, Duration) {
    let (mut brain, client) = tokio::io::duplex(64 * 1024);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(hello(&mut brain, remaining_ms));
    let pings = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}
"#
    .repeat(20);
    let (keep, withheld) = std_mpsc::channel();
    let (writing, stuck) = std_mpsc::channel();
    let (done, ended) = std_mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        done.send(run(
            move || Ok::<DuplexStream, BridgeError>(client),
            Cursor::new(pings),
            Withheld(withheld, writing),
        ))
        .unwrap_or_default();
    });
    if let BrainEnd::LostOnceStuck = end {
        stuck
            .recv_timeout(Duration::from_secs(10))
            .expect("a line reaches stdout");
        drop(brain);
    }
    let result = ended
        .recv_timeout(Duration::from_secs(10))
        .expect("the bridge ends in time");
    drop(keep);
    (result, started.elapsed())
}

#[test]
fn a_stdout_nobody_reads_holds_neither_memory_nor_the_ending() {
    let (result, took) = withheld(300, BrainEnd::Kept);
    assert!(matches!(result, Err(BridgeError::Deadline)), "{result:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");

    let (result, took) = withheld(60_000, BrainEnd::LostOnceStuck);
    assert!(
        matches!(result, Err(BridgeError::Stdout)),
        "a lost pipe, with lines unwritten: {result:?}"
    );
    assert!(took < Duration::from_secs(3), "{took:?}");
}
