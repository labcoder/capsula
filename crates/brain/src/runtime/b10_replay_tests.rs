use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kumochi_protocol::message::{ControlToShell, DataToShell, ErrorKey, Phase};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::timeout;

use super::{Catalog, FakeStore, LATER, duplex, listing, seeded};
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{Connect, ModelClient};
use crate::connections::model::error::{Code, Failure, Param, ProviderError};
use crate::coordinator::SendKey;
use crate::engines::responses::{AccessHandle, Responses};
use crate::engines::{Engine, EngineFailure, Finish, Round, RunRequest, SendGate, TurnEvent};
use crate::runtime::{Ended, Selected};
use crate::tap::Tap;
use crate::test_support::{self, Captured, Scratch};

const PROMPT: &str =
    "Use draw exactly once to draw one small rectangle, with begin, one step, and end.";
const PRIVATE_MARKER: &str = "B10_PRIVATE_PROVIDER_SENTINEL";

struct Local(SocketAddr);

impl Connect for Local {
    type Io = TcpStream;

    async fn connect(&self) -> io::Result<TcpStream> {
        assert!(self.0.ip().is_loopback());
        TcpStream::connect(self.0).await
    }
}

struct Observed<E> {
    engine: E,
    failures: Arc<Mutex<Vec<Failure>>>,
    job_finished: Arc<Notify>,
}

impl<E: std::fmt::Debug> std::fmt::Debug for Observed<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ObservedEngine")
    }
}

impl<E: Engine> Engine for Observed<E> {
    fn start(&self, request: RunRequest, events: mpsc::Sender<TurnEvent>, gate: SendGate) -> Round {
        let job = matches!(request.key, SendKey::Job(_));
        let (send, mut receive) = mpsc::channel(16);
        let round = self.engine.start(request, send, gate);
        let failures = Arc::clone(&self.failures);
        let job_finished = Arc::clone(&self.job_finished);
        Box::pin(async move {
            let forward = async {
                while let Some(event) = receive.recv().await {
                    if let TurnEvent::Finished(Finish::Failed(EngineFailure::Stream(failure))) =
                        &event
                    {
                        failures.lock().expect("metadata").push(*failure);
                    }
                    if events.send(event).await.is_err() {
                        break;
                    }
                }
            };
            tokio::join!(round, forward);
            if job {
                job_finished.notify_one();
            }
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum Case {
    Draw,
    InvalidDraw,
    BadRequest,
    RateLimited,
}

fn event(kind: &str, mut value: Value) -> String {
    value["type"] = kind.into();
    format!("data: {value}\n\n")
}

fn draw_stream(invalid: bool, request: usize) -> Vec<u8> {
    let args = if invalid {
        json!({"records":[{"step":{"id":"B10_PRIVATE_DRAW_SENTINEL"}}]})
    } else {
        json!({"records":[
            {"begin":{"v":1,"scene":"replay","surface":"overlay","alt":"replay"}},
            {"step":{"id":"s","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10]}}]}},
            {"end":{}}
        ]})
    }
    .to_string();
    let id = format!("fc_replay_{request}");
    let mut item = json!({"type":"function_call","id":id,"call_id":format!("call_replay_{request}"),"name":"draw","arguments":""});
    let mut out = event(
        "response.output_item.added",
        json!({"output_index":0,"item":item}),
    );
    out += &event(
        "response.function_call_arguments.delta",
        json!({"item_id":id,"delta":args}),
    );
    out += &event(
        "response.function_call_arguments.done",
        json!({"item_id":id,"arguments":args}),
    );
    item["arguments"] = args.into();
    out += &event(
        "response.output_item.done",
        json!({"output_index":0,"item":item}),
    );
    out += &event(
        "response.completed",
        json!({"response":{"status":"completed"}}),
    );
    out.into_bytes()
}

#[derive(Debug, Default)]
struct RequestCounts {
    requests: usize,
    draw_requests: usize,
    job_requests: usize,
    schema_bytes: usize,
    instruction_bytes: usize,
}

async fn serve(
    listener: TcpListener,
    case: Case,
    counts: Arc<Mutex<RequestCounts>>,
    mut stop: oneshot::Receiver<()>,
) {
    loop {
        let accepted =
            tokio::select! { _ = &mut stop => break, accepted = listener.accept() => accepted };
        let (mut socket, _) = accepted.expect("loopback accept");
        let mut bytes = Vec::new();
        let body = timeout(Duration::from_secs(10), async {
            loop {
                let mut block = [0; 4096];
                let n = socket.read(&mut block).await.expect("loopback request");
                assert_ne!(n, 0, "request must finish");
                bytes.extend_from_slice(&block[..n]);
                assert!(bytes.len() < 512 * 1024);
                if let Some(at) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let head = std::str::from_utf8(&bytes[..at]).expect("HTTP header");
                    let length = head
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .expect("length")
                        .parse::<usize>()
                        .expect("numeric length");
                    if bytes.len() >= at + 4 + length {
                        return serde_json::from_slice::<Value>(&bytes[at + 4..at + 4 + length])
                            .expect("JSON body");
                    }
                }
            }
        })
        .await
        .expect("request deadline");
        let tools = body["tools"].as_array().expect("tools");
        let is_job = tools.is_empty();
        assert!(body["model"] == "gpt-6-luna");
        assert!(body["reasoning"]["effort"] == "low");
        assert!(body["input"].as_array().expect("input").iter().any(|item| {
            item["content"]
                .as_str()
                .is_some_and(|text| text.contains(PROMPT))
        }));
        let requests = {
            let mut counts = counts.lock().expect("counts");
            counts.requests += 1;
            if is_job {
                counts.job_requests += 1;
            } else {
                assert_eq!(tools.len(), 1);
                assert!(tools[0]["name"] == "draw");
                assert!(tools[0]["strict"] == false);
                counts.draw_requests += 1;
                counts.schema_bytes = tools[0]["parameters"].to_string().len();
                counts.instruction_bytes =
                    body["instructions"].as_str().expect("instructions").len();
                assert_eq!(counts.schema_bytes, 30_704);
            }
            counts.draw_requests
        };
        let completed = || {
            event(
                "response.completed",
                json!({"response":{"status":"completed"}}),
            )
            .into_bytes()
        };
        let (status, media, response) = match case {
            Case::Draw => (200, "text/event-stream", if requests == 1 && !is_job { draw_stream(false, requests) } else { completed() }),
            Case::InvalidDraw => (200, "text/event-stream", match requests { 1 => draw_stream(true, requests), 2 => draw_stream(false, requests), _ => completed() }),
            Case::BadRequest => (
                400,
                "application/json",
                json!({"error":{"code":"invalid_request_error","param":format!("tools.{PRIVATE_MARKER}"),"message":PRIVATE_MARKER,"detail":PRIVATE_MARKER}}).to_string().into_bytes(),
            ),
            Case::RateLimited => (
                if requests == 1 { 429 } else { 503 },
                "application/json",
                br#"{"error":{"code":"rate_limit_error"}}"#.to_vec(),
            ),
        };
        let head = format!(
            "HTTP/1.1 {status} Replay\r\nContent-Type: {media}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        );
        socket
            .write_all(head.as_bytes())
            .await
            .expect("response head");
        socket.write_all(&response).await.expect("response");
        socket.shutdown().await.expect("response end");
    }
}

#[tokio::test]
async fn b10_loopback_replays_one_budgeted_typed_turn() {
    assert_eq!(
        format!(
            "{:?}",
            [
                Failure::Incomplete,
                Failure::Truncated,
                Failure::Protocol,
                Failure::Rebuild,
                Failure::Spent,
                Failure::Transport,
                Failure::Request,
                Failure::Credential
            ]
        ),
        "[Incomplete, Truncated, Protocol, Rebuild, Spent, Transport, Request, Credential]"
    );
    let captured = Captured::default();
    let _capture = captured.install();
    for case in [
        Case::Draw,
        Case::InvalidDraw,
        Case::BadRequest,
        Case::RateLimited,
    ] {
        timeout(Duration::from_secs(30), async {
            let scratch = Scratch::new("b10-loopback");
            let store = FakeStore::default();
            let accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("ephemeral loopback");
            let address = listener.local_addr().expect("bound socket");
            let counts = Arc::new(Mutex::new(RequestCounts::default()));
            let failures = Arc::new(Mutex::new(Vec::new()));
            let job_finished = Arc::new(Notify::new());
            let (stop, stopped) = oneshot::channel();
            let server = tokio::spawn(serve(listener, case, Arc::clone(&counts), stopped));
            let (access, asks) = AccessHandle::new();
            let engine = Observed { engine: Responses::new(ModelClient::new(Local(address)), Budget::limited(Some(3)), access), failures: Arc::clone(&failures), job_finished: Arc::clone(&job_finished) };
            let mut brain = test_support::brain_on(&scratch, Selected::Account { engine: Box::new(engine), asks }, accounts);
            let ((control, data), mut shell) = duplex();
            let mut tap = Tap::from_env();
            let client = async {
                loop { if matches!(shell.data().await, DataToShell::Models { .. }) { break; } }
                loop {
                    if matches!(shell.frame().await, ControlToShell::Settings { view, .. } if view.default_model.is_some()) { break; }
                }
                shell.submit(PROMPT).await;
                let mut steps = 0;
                let mut complete = 0;
                let run = loop {
                    match shell.data().await {
                        DataToShell::SceneStepCommit { .. } => steps += 1,
                        DataToShell::SceneEnd { complete: true, .. } => complete += 1,
                        DataToShell::RunEnd { run } => break run,
                        _ => {}
                    }
                };
                let terminal = loop {
                    if let ControlToShell::RunState { run: seen, phase, error, .. } = shell.frame().await
                        && seen == run && matches!(phase, Phase::Done | Phase::Failed | Phase::Interrupted)
                    { break (phase, error.map(|error| error.key())); }
                };
                if matches!(case, Case::Draw | Case::InvalidDraw) {
                    job_finished.notified().await;
                }
                drop(shell);
                (steps, complete, terminal)
            };
            let (ended, (steps, complete, terminal)) = tokio::join!(brain.serve(control, data, &mut tap), client);
            assert_eq!(ended, Ended::Lost);
            brain.close().await;
            stop.send(()).expect("server stop");
            server.await.expect("server joined");
            let counts = counts.lock().expect("counts");
            let failures = failures.lock().expect("failure metadata");
            match case {
                Case::Draw => { assert_eq!((counts.requests, counts.draw_requests, counts.job_requests), (3, 2, 1)); assert_eq!((steps, complete), (1, 1)); assert!(failures.is_empty()); assert_eq!(terminal, (Phase::Done, None)); }
                Case::InvalidDraw => { assert_eq!((counts.requests, counts.draw_requests, counts.job_requests), (3, 3, 0)); assert_eq!((steps, complete), (1, 1)); assert_eq!(*failures, [Failure::Spent]); assert_eq!(terminal, (Phase::Done, None)); }
                Case::BadRequest => { assert_eq!((counts.requests, counts.draw_requests, counts.job_requests), (1, 1, 0)); assert_eq!((steps, complete), (0, 0)); assert_eq!(*failures, [Failure::Provider(ProviderError { status:400, code:Some(Code::Other), param:Some(Param::Tools) })]); assert_eq!(terminal, (Phase::Failed, Some(ErrorKey::RequestRefused))); }
                Case::RateLimited => { assert_eq!((counts.requests, counts.draw_requests, counts.job_requests), (1, 1, 0)); assert_eq!((steps, complete), (0, 0)); assert_eq!(*failures, [Failure::Provider(ProviderError { status:429, code:Some(Code::Other), param:None })]); assert_eq!(terminal, (Phase::Failed, Some(ErrorKey::AccountOffline))); }
            }
            println!("case={case:?} inference_budget=3 requests={} draw_requests={} job_requests={} schema_bytes={} instruction_bytes={} steps={steps} complete_draws={complete} terminal={terminal:?} failures={failures:?}", counts.requests, counts.draw_requests, counts.job_requests, counts.schema_bytes, counts.instruction_bytes);
        }).await.expect("bounded offline replay");
    }
    let log = captured.text();
    assert!(!log.contains(PRIVATE_MARKER));
    assert!(!log.contains("B10_PRIVATE_DRAW_SENTINEL"));
    assert!(!log.contains(&format!("{:?}", PRIVATE_MARKER.as_bytes())));
    assert!(!log.contains(&format!("{:?}", b"B10_PRIVATE_DRAW_SENTINEL")));
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("responses attempt failed")
                || line.contains("responses failed before stream")
                || line.contains("draw call rejected"))
            .count(),
        4
    );
    for line in log.lines().filter(|line| {
        line.contains("responses attempt failed")
            || line.contains("responses failed before stream")
            || line.contains("draw call rejected")
    }) {
        println!("{line}");
    }
    assert!(log.contains("http_status=400"));
    assert!(log.contains("provider_param=Some(Tools)"));
    assert!(log.contains("http_status=429"));
    assert!(log.contains("draw call rejected"));
    assert!(log.contains("code=Some(BeforeBegin)"));
    assert!(log.contains("failure=Spent"));
    println!("diagnostic_marker_absent=true draw_marker_absent=true");
}
