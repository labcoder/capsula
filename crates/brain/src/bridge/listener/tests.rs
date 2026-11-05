//! Listener tests. Session tests drive `session` over an in-memory stream. The client checks use
//! this test binary, re-entered at [`child_client`], as the process that connects.

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, DuplexStream};
use tokio::net::windows::named_pipe::ClientOptions;

use super::*;
use crate::bridge::id::{JsonRpcId, MAX_STRING_ID};
use crate::bridge::wire::CHUNK;
use crate::platform::{Launch, Spawned, Stdio, create_owner_only, spawn};

const LAUNCH: LaunchId = LaunchId(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
const RUN: RunId = RunId(42);
const NOT_ADMITTED_TEXT: &str = "Kumochi isn't taking tool calls from this session now.";
const UNAVAILABLE_TEXT: &str = "This tool isn't available over MCP yet.";
const CHILD: &str = "bridge::listener::tests::child_client";
const BRIDGE_CHILD: &str = "bridge::listener::tests::child_bridge";
const PIPE_VAR: &str = "KUMOCHI_TEST_LISTENER_PIPE";
const MODE_VAR: &str = "KUMOCHI_TEST_LISTENER_MODE";

fn probe() -> ToolSchema {
    ToolSchema {
        name: "probe",
        description: "Answers nothing.",
        parameters: json!({"type": "object"}),
    }
}

struct Fixture {
    job: Job,
    exe: PathBuf,
    tools: Vec<ToolSchema>,
}

fn fixture() -> Fixture {
    Fixture {
        job: Job::new().expect("job"),
        exe: std::env::current_exe().expect("test executable"),
        tools: vec![probe()],
    }
}

impl Fixture {
    fn context(&self, left: Duration) -> LaunchContext<'_> {
        LaunchContext {
            job: &self.job,
            brain_exe: &self.exe,
            deadline: Instant::now() + left,
            tools: &self.tools,
            gate: AuditGate::new(),
            binding: Binding::default(),
        }
    }
}

/// A launch id no other test in this process uses.
fn unique_launch(n: u128) -> LaunchId {
    LaunchId(u128::from(std::process::id()) << 64 | n)
}

/// The bridge's end of a session.
struct Peer(DuplexStream);

impl Peer {
    async fn hello(&mut self) -> u64 {
        match crate::ipc::read::<ToBridge>(&mut self.0, Pipe::Data).await {
            Ok(Some(ToBridge::Hello { remaining_ms })) => remaining_ms,
            other => panic!("{other:?}"),
        }
    }

    async fn frame(&mut self, message: &ToBrain) {
        let frame = frame::encode(message, Pipe::Data).expect("encodes");
        self.0.write_all(&frame).await.expect("write");
    }

    async fn request(&mut self, key: &CallKey, method: Method, params: &[u8]) {
        for (chunk, last) in chunks(params) {
            self.frame(&ToBrain::Request {
                key: key.clone(),
                method,
                chunk: chunk.to_vec(),
                last,
            })
            .await;
        }
    }

    /// The next reply, reassembled: its key, its body and whether it's an error.
    async fn reply(&mut self) -> (CallKey, Value, bool) {
        let mut body = Vec::new();
        loop {
            match crate::ipc::read::<ToBridge>(&mut self.0, Pipe::Data).await {
                Ok(Some(ToBridge::Reply {
                    key,
                    chunk,
                    last,
                    error,
                })) => {
                    body.extend(chunk);
                    if last {
                        return (key, serde_json::from_slice(&body).expect("json"), error);
                    }
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// Waits for the session to drop its end, with nothing more sent.
    async fn ends(mut self) {
        let mut rest = Vec::new();
        self.0.read_to_end(&mut rest).await.expect("read");
        assert_eq!(rest, Vec::<u8>::new());
    }
}

async fn with_session<F: Future<Output = ()>>(
    context: &LaunchContext<'_>,
    drive: impl FnOnce(Peer) -> F,
) -> SessionEnd {
    let (ours, theirs) = tokio::io::duplex(4 << 20);
    let (end, ()) = tokio::join!(session(ours, LAUNCH, context), drive(Peer(theirs)));
    end
}

fn key(n: i64) -> CallKey {
    JsonRpcId::Number(n).key()
}

fn call(name: &str) -> Vec<u8> {
    json!({"name": name, "arguments": {}})
        .to_string()
        .into_bytes()
}

fn text_result(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

fn invalid_params() -> Value {
    json!({"code": -32602, "message": "Invalid params"})
}

#[tokio::test]
async fn methods_answer_from_the_frozen_tool_set() {
    let fixture = fixture();
    let context = fixture.context(Duration::from_secs(60));
    let end = with_session(&context, |mut peer| async move {
        let left = peer.hello().await;
        assert!((59_000..=60_000).contains(&left), "{left}");
        for (asked, answered) in [("2025-03-26", "2025-03-26"), ("1999-01-01", "2025-06-18")] {
            let params = json!({"protocolVersion": asked, "capabilities": {}}).to_string();
            peer.request(&key(1), Method::Initialize, params.as_bytes()).await;
            assert_eq!(
                peer.reply().await,
                (
                    key(1),
                    json!({
                        "protocolVersion": answered,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": MCP_SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                    }),
                    false
                )
            );
        }
        peer.request(&key(2), Method::ToolsList, b"null").await;
        assert_eq!(
            peer.reply().await,
            (
                key(2),
                json!({"tools": [{
                    "name": "probe",
                    "description": "Answers nothing.",
                    "inputSchema": {"type": "object"},
                }]}),
                false
            )
        );
        for params in [
            call("draw"),
            br#"["probe", {}]"#.to_vec(),
            br#"{"name":"probe","arguments":5}"#.to_vec(),
            br#"{"name":"probe","arguments":{},"extra":1}"#.to_vec(),
            br#"{"arguments":{}}"#.to_vec(),
            b"not json".to_vec(),
        ] {
            peer.request(&key(3), Method::ToolsCall, &params).await;
            assert_eq!(
                peer.reply().await,
                (key(3), invalid_params(), true),
                "{}",
                String::from_utf8_lossy(&params)
            );
        }
        let meta = br#"{"name":"probe","_meta":{"progressToken":1}}"#;
        peer.request(&key(4), Method::ToolsCall, meta).await;
        assert_eq!(peer.reply().await, (key(4), text_result(NOT_ADMITTED_TEXT), false));
    })
    .await;
    assert_eq!(end, SessionEnd::Closed);
}

#[tokio::test]
async fn a_call_needs_a_bound_run_and_a_passed_gate_and_a_failure_stays() {
    let fixture = fixture();
    let context = fixture.context(Duration::from_secs(60));
    let (gate, binding) = (context.gate.clone(), context.binding.clone());
    let end = with_session(&context, |mut peer| async move {
        peer.hello().await;
        let mut ask = async |expected: &str| {
            peer.request(&key(9), Method::ToolsCall, &call("probe"))
                .await;
            assert_eq!(peer.reply().await, (key(9), text_result(expected), false));
        };
        ask(NOT_ADMITTED_TEXT).await;
        gate.pass();
        ask(NOT_ADMITTED_TEXT).await;
        binding.bind(RUN).expect("first bind");
        ask(UNAVAILABLE_TEXT).await;
        gate.fail();
        ask(NOT_ADMITTED_TEXT).await;
        gate.pass();
        ask(NOT_ADMITTED_TEXT).await;
    })
    .await;
    assert_eq!(end, SessionEnd::Closed);
}

#[test]
fn an_origin_is_frozen_at_admission_and_belongs_to_its_launch() {
    let fixture = fixture();
    let context = fixture.context(Duration::from_secs(60));
    context.binding.bind(RUN).expect("first bind");
    context.gate.pass();
    let call = key(7);
    let origin = McpOrigin {
        run: RUN,
        launch: LAUNCH,
        key: key(7),
    };
    assert_eq!(context.admit(LAUNCH, &call), Ok(origin.clone()));
    assert_eq!(context.binding.bind(RunId(43)), Err(AlreadyBound));
    let admitted = context.admit(LAUNCH, &call);
    context.gate.fail();
    assert_eq!(
        admitted,
        Ok(origin),
        "a later failure doesn't undo an admission"
    );
    assert_eq!(context.admit(LAUNCH, &call), Err(NotAdmitted));

    // The next launch has its own gate and binding; nothing carries over.
    let next = fixture.context(Duration::from_secs(60));
    assert_eq!(next.admit(LAUNCH, &call), Err(NotAdmitted));
    next.binding.bind(RUN).expect("first bind");
    assert_eq!(
        next.admit(LAUNCH, &call),
        Err(NotAdmitted),
        "its gate is still pending"
    );

    let late = fixture.context(Duration::ZERO);
    late.binding.bind(RUN).expect("first bind");
    late.gate.pass();
    assert_eq!(late.admit(LAUNCH, &call), Err(NotAdmitted));
}

#[tokio::test]
async fn a_request_of_exactly_the_cap_passes_and_one_over_gets_one_error() {
    let fixture = fixture();
    let context = fixture.context(Duration::from_secs(60));
    let shell = r#"{"name":"probe","arguments":{"t":""}}"#;
    let sized = |len: usize| {
        let pad = "a".repeat(len - shell.len());
        format!(r#"{{"name":"probe","arguments":{{"t":"{pad}"}}}}"#).into_bytes()
    };
    let end = with_session(&context, |mut peer| async move {
        peer.hello().await;
        let exact = sized(MAX_INCOMING);
        assert_eq!((exact.len(), chunks(&exact).count()), (MAX_INCOMING, 3));
        peer.request(&key(1), Method::ToolsCall, &exact).await;
        assert_eq!(
            peer.reply().await,
            (key(1), text_result(NOT_ADMITTED_TEXT), false)
        );
        peer.request(&key(2), Method::ToolsCall, &sized(MAX_INCOMING + 1))
            .await;
        assert_eq!(
            peer.reply().await,
            (
                key(2),
                json!({"code": -32603, "message": "Internal error"}),
                true
            )
        );
        peer.request(&key(3), Method::ToolsList, b"null").await;
        assert_eq!(peer.reply().await.0, key(3), "the session goes on");
    })
    .await;
    assert_eq!(end, SessionEnd::Closed);
}

/// A first chunk that isn't the last.
fn chunk(key: &CallKey, method: Method) -> ToBrain {
    ToBrain::Request {
        key: key.clone(),
        method,
        chunk: b"{".to_vec(),
        last: false,
    }
}

#[tokio::test]
async fn a_chunk_that_doesnt_continue_its_request_breaks_the_session() {
    let fixture = fixture();
    let cases: [(&str, Vec<ToBrain>); 4] = [
        (
            "another key",
            vec![
                chunk(&key(1), Method::ToolsCall),
                chunk(&key(2), Method::ToolsCall),
            ],
        ),
        (
            "another method",
            vec![
                chunk(&key(1), Method::ToolsCall),
                chunk(&key(1), Method::ToolsList),
            ],
        ),
        (
            "initialized inside a request",
            vec![chunk(&key(1), Method::ToolsCall), ToBrain::Initialized],
        ),
        (
            "the pipe's end inside a request",
            vec![chunk(&key(1), Method::ToolsCall)],
        ),
    ];
    for (case, frames) in cases {
        let context = fixture.context(Duration::from_secs(60));
        let end = with_session(&context, |mut peer| async move {
            peer.hello().await;
            for message in &frames {
                peer.frame(message).await;
            }
            if case.starts_with("the pipe's end") {
                return;
            }
            peer.ends().await;
        })
        .await;
        assert_eq!(end, SessionEnd::Broken, "{case}");
    }
}

#[tokio::test]
async fn only_keys_the_bridge_can_make_start_a_request() {
    let fixture = fixture();
    let too_long = format!("m:s:{}", "61".repeat(MAX_STRING_ID + 1));
    for bad in [
        "m:x:1",
        "m:n:01",
        "m:n:-0",
        "m:n:9223372036854775808",
        "m:n:",
        "m:s:abc",
        "m:s:AB",
        too_long.as_str(),
    ] {
        let context = fixture.context(Duration::from_secs(60));
        let end = with_session(&context, |mut peer| async move {
            peer.hello().await;
            peer.request(&CallKey(bad.to_owned()), Method::ToolsList, b"null")
                .await;
            peer.ends().await;
        })
        .await;
        assert_eq!(end, SessionEnd::Broken, "{bad}");
    }
    // 29 bytes: fourteen two-byte characters and one more.
    let unicode = format!("{}a", "é".repeat(14));
    assert_eq!(unicode.len(), MAX_STRING_ID);
    let context = fixture.context(Duration::from_secs(60));
    let end = with_session(&context, |mut peer| async move {
        peer.hello().await;
        for good in [
            CallKey("m:n:5".to_owned()),
            CallKey("m:n:-5".to_owned()),
            CallKey("m:n:-9223372036854775808".to_owned()),
            CallKey("m:s:".to_owned()),
            JsonRpcId::String(unicode).key(),
        ] {
            peer.request(&good, Method::ToolsList, b"null").await;
            assert_eq!(peer.reply().await.0, good);
        }
    })
    .await;
    assert_eq!(end, SessionEnd::Closed);
}

#[tokio::test]
async fn the_deadline_and_a_malformed_frame_end_the_session() {
    let fixture = fixture();
    let context = fixture.context(Duration::from_millis(300));
    let end = with_session(&context, |mut peer| async move {
        peer.hello().await;
        peer.ends().await;
    })
    .await;
    assert_eq!(end, SessionEnd::Deadline);

    let context = fixture.context(Duration::from_millis(300));
    let end = with_session(&context, |mut peer| async move {
        peer.hello().await;
        // Over the cap and not yet last: the deadline still ends the drain.
        let over = vec![b'a'; MAX_INCOMING + CHUNK];
        for (part, _) in chunks(&over) {
            peer.frame(&ToBrain::Request {
                key: key(1),
                method: Method::ToolsCall,
                chunk: part.to_vec(),
                last: false,
            })
            .await;
        }
        peer.ends().await;
    })
    .await;
    assert_eq!(end, SessionEnd::Deadline);

    let context = fixture.context(Duration::from_secs(60));
    let end = with_session(&context, |mut peer| async move {
        peer.hello().await;
        let mut bad = frame::encode(&ToBrain::Initialized, Pipe::Data).expect("encodes");
        *bad.last_mut().expect("a payload") = 9;
        peer.0.write_all(&bad).await.expect("write");
        peer.ends().await;
    })
    .await;
    assert_eq!(end, SessionEnd::Broken);
}

#[tokio::test]
async fn nothing_the_client_sent_reaches_debug_or_an_answer() {
    const PLANTED: &str = "kumochi-planted";
    // "kumo" as a byte list and as the hex a call key spells.
    let (decimal, hex) = ("107, 117, 109, 111", "6b756d6f");
    let fixture = fixture();
    let context = fixture.context(Duration::from_secs(60));
    let planted = JsonRpcId::String(PLANTED.to_owned()).key();
    let call_key = planted.clone();
    let mut answered = None;
    let slot = &mut answered;
    let end = with_session(&context, |mut peer| async move {
        peer.hello().await;
        let params = json!({"name": PLANTED, "arguments": {"x": PLANTED}}).to_string();
        peer.request(&call_key, Method::ToolsCall, params.as_bytes())
            .await;
        *slot = Some(peer.reply().await);
    })
    .await;
    let answer = answered.expect("answered");
    assert_eq!(answer, (planted.clone(), invalid_params(), true));
    let printed = [
        format!(
            "{:?}",
            McpOrigin {
                run: RUN,
                launch: LAUNCH,
                key: planted
            }
        ),
        format!("{end:?}"),
        answer.1.to_string(),
    ];
    for text in printed {
        for needle in [PLANTED, decimal, hex] {
            assert!(!text.contains(needle), "{needle} in {text}");
        }
    }
}

#[tokio::test]
async fn the_name_takes_one_instance_and_one_client() {
    let listener = Listener::create(unique_launch(1)).expect("listener");
    let code = |result: io::Result<_>| result.err().and_then(|error| error.raw_os_error());
    assert_eq!(
        code(Listener::create(unique_launch(1)).map(|_| ())),
        Some(231),
        "taken"
    );
    assert_eq!(
        code(create_owner_only(listener.name(), Instance::Next).map(|_| ())),
        Some(231),
        "a second instance without the first-instance flag"
    );
    let _first = ClientOptions::new()
        .open(listener.name())
        .expect("first client");
    assert_eq!(
        code(ClientOptions::new().open(listener.name()).map(|_| ())),
        Some(231)
    );
}

#[tokio::test]
async fn a_refused_client_reads_nothing_and_the_name_is_gone_after() {
    let fixture = fixture();
    let listener = Listener::create(unique_launch(2)).expect("listener");
    let name = listener.name().to_owned();
    let mut client = ClientOptions::new().open(&name).expect("client");
    let end = listener
        .serve(fixture.context(Duration::from_secs(10)))
        .await;
    assert_eq!(
        end,
        SessionEnd::Refused(ClientRefused::NotInJob),
        "this process is outside the job"
    );
    let mut read = Vec::new();
    client.read_to_end(&mut read).await.unwrap_or_default();
    assert_eq!(read, Vec::<u8>::new());
    assert_eq!(
        ClientOptions::new()
            .open(&name)
            .err()
            .and_then(|error| error.raw_os_error()),
        Some(2),
        "nothing listens on the name again"
    );

    let listener = Listener::create(unique_launch(3)).expect("listener");
    let end = listener
        .serve(fixture.context(Duration::from_millis(200)))
        .await;
    assert_eq!(end, SessionEnd::NoClient);
}

fn harness(test: &str) -> Vec<OsString> {
    [
        "--exact",
        test,
        "--ignored",
        "--nocapture",
        "--test-threads=1",
        "-q",
    ]
    .map(OsString::from)
    .to_vec()
}

/// This test binary as a client of `pipe`, in `jobs`.
fn start_client(test: &str, pipe: &str, mode: &str, jobs: &[&Job]) -> Spawned {
    let exe = std::env::current_exe().expect("test executable");
    spawn(&Launch {
        exe: &exe,
        args: &harness(test),
        cwd: exe.parent().expect("folder"),
        env: &[
            (
                "SystemRoot".into(),
                std::env::var_os("SystemRoot").expect("SystemRoot is set"),
            ),
            (PIPE_VAR.into(), pipe.into()),
            (MODE_VAR.into(), mode.into()),
        ],
        stdio: Stdio::Piped,
        jobs,
    })
    .expect("spawn")
}

/// The client's stdout once it has exited.
#[track_caller]
fn output(mut child: Spawned) -> String {
    assert!(
        child
            .process
            .exits_within(Duration::from_secs(30))
            .expect("wait"),
        "the client ends"
    );
    let mut text = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut text)
        .expect("stdout");
    text
}

#[test]
#[ignore = "a client process for the listener's tests; see start_client"]
fn child_client() {
    let (Some(name), Some(mode)) = (std::env::var_os(PIPE_VAR), std::env::var_os(MODE_VAR)) else {
        return;
    };
    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name)
        .expect("open");
    if mode == "leave" {
        return;
    }
    match pipe.read(&mut [0; 1]) {
        Ok(1) => println!("client: frame"),
        _ => println!("client: nothing"),
    }
}

#[tokio::test]
async fn only_a_live_brain_image_in_the_launch_job_is_served() {
    let fixture = fixture();
    let (parent, other) = (Job::new().expect("job"), Job::new().expect("job"));

    let listener = Listener::create(unique_launch(4)).expect("listener");
    let child = start_client(CHILD, listener.name(), "read", &[&fixture.job]);
    assert_eq!(
        listener
            .serve(fixture.context(Duration::from_secs(10)))
            .await,
        SessionEnd::Closed
    );
    assert!(output(child).contains("client: frame"));

    let listener = Listener::create(unique_launch(5)).expect("listener");
    let child = start_client(CHILD, listener.name(), "read", &[&parent, &other]);
    assert_eq!(
        listener
            .serve(fixture.context(Duration::from_secs(10)))
            .await,
        SessionEnd::Refused(ClientRefused::NotInJob),
        "another launch's job"
    );
    assert!(output(child).contains("client: nothing"));

    let listener = Listener::create(unique_launch(6)).expect("listener");
    let child = start_client(CHILD, listener.name(), "read", &[&fixture.job]);
    let elsewhere = Path::new(r"C:\elsewhere\kumochid.exe");
    let context = LaunchContext {
        brain_exe: elsewhere,
        ..fixture.context(Duration::from_secs(10))
    };
    assert_eq!(
        listener.serve(context).await,
        SessionEnd::Refused(ClientRefused::NotOurImage)
    );
    assert!(output(child).contains("client: nothing"));

    // A client that connected and exited while its process object is still held still opens
    // by its pid, so only the liveness check refuses it.
    let listener = Listener::create(unique_launch(7)).expect("listener");
    let (connected, child) = tokio::join!(listener.server.connect(), async {
        start_client(CHILD, listener.name(), "leave", &[&fixture.job])
    });
    connected.expect("connects");
    assert!(
        child
            .process
            .exits_within(Duration::from_secs(30))
            .expect("wait")
    );
    let context = fixture.context(Duration::from_secs(10));
    assert_eq!(
        check_client(&listener.server, &context).err(),
        Some(ClientRefused::Exited)
    );
}

#[test]
#[ignore = "runs as the bridge for measure_mcp_round_trip"]
fn child_bridge() {
    let Some(name) = std::env::var_os(PIPE_VAR).and_then(|name| name.into_string().ok()) else {
        return;
    };
    let own = std::env::current_exe().expect("test executable");
    let result = crate::bridge::run(
        || crate::bridge::connect(&name, &own),
        std::io::stdin(),
        std::io::stdout(),
    );
    assert!(matches!(result, Ok(())), "{result:?}");
}

/// The MCP round trip from a stdin line through the bridge, the pipe and the listener back to
/// stdout, 1,000 times. Run alone, on a quiet Rez under the measuring lock:
/// `cargo test -p kumochi-brain --release -- --ignored --exact bridge::listener::tests::measure_mcp_round_trip --nocapture`.
/// Leaves out claude.exe and any tool's cost: every call is admitted and answered `Unavailable`.
#[test]
#[ignore = "a measurement: run alone on Rez under the measuring lock"]
fn measure_mcp_round_trip() {
    const WARMUP: usize = 50;
    const SAMPLES: usize = 1_000;
    let fixture = fixture();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let listener = {
        let _inside = runtime.enter();
        Listener::create(unique_launch(8)).expect("listener")
    };
    let started = std::time::Instant::now();
    let mut child = start_client(BRIDGE_CHILD, listener.name(), "bridge", &[&fixture.job]);
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (line, lines) = std_mpsc::channel();
    std::thread::spawn(move || {
        for text in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.send(text).is_err() {
                break;
            }
        }
    });
    let context = fixture.context(Duration::from_secs(120));
    context.binding.bind(RUN).expect("first bind");
    context.gate.pass();
    let request = |id: usize| {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"probe","arguments":{{}}}}}}"#
        )
    };
    let expected =
        |id: usize| json!({"jsonrpc": "2.0", "id": id, "result": text_result(UNAVAILABLE_TEXT)});
    let next = || {
        lines
            .recv_timeout(Duration::from_secs(5))
            .expect("a stdout line in time")
    };
    let (cold, mut samples, reply_len) = std::thread::scope(|scope| {
        let serving = scope.spawn(|| runtime.block_on(listener.serve(context)));
        let mut send = |text: &str| {
            stdin.write_all(text.as_bytes()).expect("stdin");
            stdin.write_all(b"\n").expect("stdin");
            stdin.flush().expect("stdin");
        };
        send(
            r#"{"jsonrpc":"2.0","id":"i","method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        send(r#"{"jsonrpc":"2.0","id":"l","method":"tools/list"}"#);
        // The harness prints its own lines first; skip them, and the initialize reply.
        while !next().starts_with(r#"{"jsonrpc":"2.0","id":"l""#) {}
        let cold = started.elapsed();
        let mut samples = Vec::with_capacity(SAMPLES);
        let mut reply_len = 0;
        for id in 0..WARMUP + SAMPLES {
            let line = request(id);
            let at = std::time::Instant::now();
            send(&line);
            let reply = next();
            let took = at.elapsed();
            assert_eq!(
                serde_json::from_str::<Value>(&reply).expect("json"),
                expected(id)
            );
            reply_len = reply.len() + 1;
            if id >= WARMUP {
                samples.push(took);
            }
        }
        drop(stdin);
        assert_eq!(serving.join().expect("joins"), SessionEnd::Closed);
        (cold, samples, reply_len)
    });
    samples.sort_unstable();
    // Nearest rank: the smallest sample with at least `percent` of them at or under it.
    let rank = |percent: usize| samples[(percent * SAMPLES).div_ceil(100) - 1];
    println!(
        "n={SAMPLES} warmup={WARMUP} request={}B reply={reply_len}B p50={:?} p95={:?} p99={:?} max={:?} cold={cold:?}",
        request(WARMUP + SAMPLES - 1).len() + 1,
        rank(50),
        rank(95),
        rank(99),
        samples[SAMPLES - 1],
    );
}
