//! Listener tests. Session tests drive `session` over an in-memory stream. The client checks use
//! this test binary, re-entered at [`child_client`], as the process that connects.

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use tokio::net::windows::named_pipe::ClientOptions;

use kumochi_protocol::message::RunId;

use super::*;
use crate::bridge::id::{JsonRpcId, MAX_STRING_ID};
use crate::bridge::wire::CHUNK;
use crate::platform::{Launch, Spawned, Stdio, create_owner_only, spawn};

const LAUNCH: LaunchId = LaunchId(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
const BUILD: BuildKey = BuildKey {
    run: RunId(42),
    round: 1,
    attempt: 1,
};
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
            calls: None,
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

/// `BUILD`, bound until a minute from now.
fn bound() -> (BuildKey, Instant) {
    (BUILD, Instant::now() + Duration::from_secs(60))
}

fn call(name: &str) -> Vec<u8> {
    json!({"name": name, "arguments": {}})
        .to_string()
        .into_bytes()
}

fn text_result(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

/// The frozen tool set's `tools/list` result.
fn probe_list() -> Value {
    json!({"tools": [{
        "name": "probe",
        "description": "Answers nothing.",
        "inputSchema": {"type": "object"},
    }]})
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
        assert_eq!(peer.reply().await, (key(2), probe_list(), false));
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
        binding.bind(Some(bound())).expect("first bind");
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
    let until = Instant::now() + Duration::from_secs(30);
    context
        .binding
        .bind(Some((BUILD, until)))
        .expect("first bind");
    context.gate.pass();
    let call = key(7);
    let origin = McpOrigin {
        build: BUILD,
        launch: LAUNCH,
        key: key(7),
    };
    assert_eq!(
        context.admit(LAUNCH, &call),
        Ok((origin.clone(), until)),
        "content leaves by the bound deadline"
    );
    assert_eq!(context.binding.bind(None), Err(AlreadyBound));
    let admitted = context.admit(LAUNCH, &call);
    context.gate.fail();
    assert_eq!(
        admitted,
        Ok((origin, until)),
        "a later failure doesn't undo an admission"
    );
    assert_eq!(context.admit(LAUNCH, &call), Err(NotAdmitted));

    // The next launch has its own gate and binding; nothing carries over.
    let next = fixture.context(Duration::from_secs(60));
    assert_eq!(next.admit(LAUNCH, &call), Err(NotAdmitted));
    next.binding.bind(Some(bound())).expect("first bind");
    assert_eq!(
        next.admit(LAUNCH, &call),
        Err(NotAdmitted),
        "its gate is still pending"
    );

    let late = fixture.context(Duration::ZERO);
    late.binding.bind(Some(bound())).expect("first bind");
    late.gate.pass();
    assert_eq!(late.admit(LAUNCH, &call), Err(NotAdmitted));

    let expired = fixture.context(Duration::from_secs(60));
    expired
        .binding
        .bind(Some((BUILD, Instant::now())))
        .expect("first bind");
    expired.gate.pass();
    assert_eq!(
        expired.admit(LAUNCH, &call),
        Err(NotAdmitted),
        "past its question's deadline, with the launch's still ahead"
    );
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
        "m:s:ff",
        "m:s:c3",
        "m:s:c0af",
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
                build: BUILD,
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
pub(crate) fn output(mut child: Spawned) -> String {
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

pub(crate) fn bridge_with_live_cli(job: &Job) -> (LaunchId, Listener, Spawned, Spawned) {
    let launch = unique_launch(10);
    let listener = Listener::create(launch).expect("listener");
    let cli = start_client(CHILD, listener.name(), "cli", &[job]);
    let bridge = start_client(BRIDGE_CHILD, listener.name(), "bridge", &[job]);
    (launch, listener, bridge, cli)
}

#[test]
#[ignore = "a client process for the listener's tests; see start_client"]
fn child_client() {
    let (Some(name), Some(mode)) = (std::env::var_os(PIPE_VAR), std::env::var_os(MODE_VAR)) else {
        return;
    };
    if mode == "cli" {
        assert_eq!(std::io::stdin().read(&mut [0]).expect("held CLI stdin"), 0);
        return;
    }
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
    context.binding.bind(Some(bound())).expect("first bind");
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
        let parsed = |line: &str| serde_json::from_str::<Value>(line).expect("json");
        send(
            r#"{"jsonrpc":"2.0","id":"i","method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        // libtest's own start lines come first; nothing else is skipped.
        let initialized = loop {
            let line = next();
            if !line.is_empty() && line != "running 1 test" {
                break line;
            }
        };
        assert_eq!(
            parsed(&initialized),
            json!({"jsonrpc": "2.0", "id": "i", "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": MCP_SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
            }})
        );
        send(r#"{"jsonrpc":"2.0","id":"l","method":"tools/list"}"#);
        assert_eq!(
            parsed(&next()),
            json!({"jsonrpc": "2.0", "id": "l", "result": probe_list()})
        );
        let cold = started.elapsed();
        let mut samples = Vec::with_capacity(SAMPLES);
        let mut reply_len = 0;
        for id in 0..WARMUP + SAMPLES {
            let line = request(id);
            let at = std::time::Instant::now();
            send(&line);
            let reply = next();
            let took = at.elapsed();
            assert_eq!(parsed(&reply), expected(id));
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

const ENDED_TEXT: &str = "This call ended before it finished.";

/// A tool result the model reads as a success.
fn succeeded(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

/// A bound, passed context that hands admitted calls to `calls`.
fn calling(fixture: &Fixture, calls: mpsc::Sender<McpCall>) -> LaunchContext<'_> {
    calling_until(fixture, calls, Instant::now() + Duration::from_secs(60))
}

/// As [`calling`], with the launch's own deadline a minute away and its question's at `until`.
fn calling_until(
    fixture: &Fixture,
    calls: mpsc::Sender<McpCall>,
    until: Instant,
) -> LaunchContext<'_> {
    let context = LaunchContext {
        calls: Some(calls),
        ..fixture.context(Duration::from_secs(60))
    };
    context
        .binding
        .bind(Some((BUILD, until)))
        .expect("first bind");
    context.gate.pass();
    context
}

#[tokio::test]
async fn an_admitted_call_reaches_the_brain_as_written_and_gets_one_reply() {
    let fixture = fixture();
    let (calls, mut brain) = mpsc::channel(1);
    let context = calling(&fixture, calls);
    let brain = tokio::spawn(async move {
        let first = brain.recv().await.expect("the first call");
        let origin = McpOrigin {
            build: BUILD,
            launch: LAUNCH,
            key: key(5),
        };
        assert_eq!(first.origin, origin);
        assert_eq!(first.tool, ToolName::Probe);
        assert_eq!(
            first.args.as_deref(),
            Some(&br#"{"word" : "h\u0069", "word":"x"}"#[..])
        );
        let probed = McpReply {
            text: "probed".to_owned(),
            error: false,
        };
        first.reply.send(probed).expect("waiting");
        let second = brain.recv().await.expect("the second call");
        assert_eq!(second.args.as_deref(), Some(&b"{}"[..]));
        drop(second);
        let third = brain.recv().await.expect("the third call");
        let refused = McpReply {
            text: "not here".to_owned(),
            error: true,
        };
        third.reply.send(refused).expect("waiting");
    });
    let end = with_session(&context, |mut peer| async move {
        peer.hello().await;
        let written = br#"{"name":"probe","arguments": {"word" : "h\u0069", "word":"x"} }"#;
        peer.request(&key(5), Method::ToolsCall, written).await;
        assert_eq!(peer.reply().await, (key(5), succeeded("probed"), false));
        peer.request(&key(6), Method::ToolsCall, br#"{"name":"probe"}"#)
            .await;
        assert_eq!(peer.reply().await, (key(6), text_result(ENDED_TEXT), false));
        peer.request(&key(7), Method::ToolsCall, &call("probe"))
            .await;
        assert_eq!(peer.reply().await, (key(7), text_result("not here"), false));
        for params in [
            br#"{"name":"probe","arguments":null}"#.to_vec(),
            br#"{"name":"probe","name":"probe","arguments":{}}"#.to_vec(),
            br#"{"name":"probe","arguments":[]}"#.to_vec(),
        ] {
            peer.request(&key(8), Method::ToolsCall, &params).await;
            assert_eq!(
                peer.reply().await,
                (key(8), invalid_params(), true),
                "{}",
                String::from_utf8_lossy(&params)
            );
        }
    })
    .await;
    assert_eq!(end, SessionEnd::Closed);
    brain.await.expect("the brain's checks");
}

/// Claude Code's timeouts can't fire while a launch lives (the policy's pins), and a repeat
/// under a new id while the first call is unsettled is refused by the bridge: one call reaches
/// the brain.
#[tokio::test]
async fn a_second_request_while_one_is_unsettled_never_reaches_the_brain() {
    let fixture = fixture();
    let (calls, mut brain) = mpsc::channel(1);
    let context = calling(&fixture, calls);
    let (ours, theirs) = tokio::io::duplex(4 << 20);
    let (lines, input) = mpsc::channel(16);
    let (_stdin, stdin_end) = tokio::sync::watch::channel(false);
    let (out, mut written) = mpsc::channel::<Vec<u8>>(16);
    let request = |id: u32| {
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"probe","arguments":{{}}}}}}"#
        );
        crate::bridge::Input::Line(crate::bridge::stdio::Line::Text(line.into_bytes()))
    };
    let drive = async move {
        let mut line = async || -> Value {
            serde_json::from_slice(&written.recv().await.expect("a line")).expect("json")
        };
        lines.send(request(1)).await.expect("relayed");
        let first = brain.recv().await.expect("the first call");
        assert_eq!(first.origin.key, key(1));
        lines.send(request(2)).await.expect("relayed");
        assert_eq!(
            line().await,
            json!({"jsonrpc":"2.0","id":2,"error":{"code":-32600,"message":"Invalid Request"}})
        );
        assert!(
            brain.try_recv().is_err(),
            "the repeat reached no dispatcher"
        );
        let drawn = McpReply {
            text: "answered once".to_owned(),
            error: false,
        };
        first.reply.send(drawn).expect("waiting");
        assert_eq!(
            line().await,
            json!({"jsonrpc":"2.0","id":1,"result":succeeded("answered once")})
        );
        lines
            .send(crate::bridge::Input::End)
            .await
            .expect("relayed");
    };
    let (end, relayed, ()) = tokio::join!(
        session(ours, LAUNCH, &context),
        crate::bridge::relay(theirs, input, stdin_end, out),
        drive
    );
    assert_eq!(end, SessionEnd::Closed);
    assert!(matches!(relayed, Ok(())));
}

#[test]
fn a_call_and_its_reply_debug_no_arguments_text_or_request_id() {
    const PLANTED: &str = "zq-mcp-planted";
    let (reply, _answer) = oneshot::channel();
    let call = McpCall {
        origin: McpOrigin {
            build: BUILD,
            launch: LAUNCH,
            key: CallKey(PLANTED.to_owned()),
        },
        tool: ToolName::Probe,
        args: Some(PLANTED.as_bytes().to_vec()),
        reply,
    };
    let answer = McpReply {
        text: PLANTED.to_owned(),
        error: true,
    };
    let bytes = format!("{:?}", PLANTED.as_bytes());
    for printed in [format!("{call:?}"), format!("{answer:?}")] {
        assert!(!printed.contains(PLANTED), "{printed}");
        assert!(
            !printed.contains(bytes.trim_matches(['[', ']'])),
            "{printed}"
        );
    }
}

#[tokio::test]
async fn arguments_at_the_cap_reach_the_brain_as_written_and_one_byte_over_stay_out_of_the_mailbox()
{
    let fixture = fixture();
    let (calls, mut brain) = mpsc::channel(1);
    let context = calling(&fixture, calls);
    let (ours, theirs) = tokio::io::duplex(4 << 20);
    let (lines, input) = mpsc::channel(16);
    let (_stdin, stdin_end) = tokio::sync::watch::channel(false);
    let (out, mut written) = mpsc::channel::<Vec<u8>>(16);
    let arguments = |len: usize| format!(r#"{{"word":"{}"}}"#, "w".repeat(len - 11));
    let request = |id: u32, arguments: &str| {
        let line = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"probe","arguments":{arguments}}}}}"#
        );
        crate::bridge::Input::Line(crate::bridge::stdio::Line::Text(line.into_bytes()))
    };
    let drive = async move {
        let mut line = async || -> Value {
            serde_json::from_slice(&written.recv().await.expect("a line")).expect("json")
        };
        let at_cap = arguments(16_384);
        assert_eq!(at_cap.len(), crate::dispatch::MAX_ARGS);
        lines.send(request(1, &at_cap)).await.expect("relayed");
        let call = brain.recv().await.expect("the call at the cap");
        assert_eq!(call.args.as_deref(), Some(at_cap.as_bytes()));
        let probed = McpReply {
            text: "probed".to_owned(),
            error: false,
        };
        call.reply.send(probed).expect("waiting");
        assert_eq!(
            line().await,
            json!({"jsonrpc":"2.0","id":1,"result":succeeded("probed")})
        );
        lines
            .send(request(2, &arguments(16_385)))
            .await
            .expect("relayed");
        let call = brain.recv().await.expect("the call over the cap");
        assert_eq!(call.args, None, "only its size reaches the brain");
        drop(call);
        assert_eq!(
            line().await,
            json!({"jsonrpc":"2.0","id":2,"result":text_result(ENDED_TEXT)})
        );
        lines
            .send(crate::bridge::Input::End)
            .await
            .expect("relayed");
    };
    let (end, relayed, ()) = tokio::join!(
        session(ours, LAUNCH, &context),
        crate::bridge::relay(theirs, input, stdin_end, out),
        drive
    );
    assert_eq!(end, SessionEnd::Closed);
    assert!(matches!(relayed, Ok(())));
}

#[tokio::test]
async fn an_answer_for_a_launch_whose_gate_failed_meanwhile_never_leaves() {
    let fixture = fixture();
    let (calls, mut brain) = mpsc::channel(1);
    let context = calling(&fixture, calls);
    let gate = context.gate.clone();
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (lines, input) = mpsc::channel(16);
    let (_stdin, stdin_end) = tokio::sync::watch::channel(false);
    let (out, mut written) = mpsc::channel::<Vec<u8>>(16);
    let drive = async move {
        let line = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"probe","arguments":{}}}"#;
        lines
            .send(crate::bridge::Input::Line(
                crate::bridge::stdio::Line::Text(line.to_vec()),
            ))
            .await
            .expect("relayed");
        let call = brain.recv().await.expect("admitted while the gate passed");
        gate.fail();
        let probed = McpReply {
            text: "probed".to_owned(),
            error: false,
        };
        call.reply.send(probed).expect("waiting");
        let answer: Value =
            serde_json::from_slice(&written.recv().await.expect("a line")).expect("json");
        assert_eq!(
            answer,
            json!({"jsonrpc":"2.0","id":1,"result":text_result(ENDED_TEXT)})
        );
        lines
            .send(crate::bridge::Input::End)
            .await
            .expect("relayed");
    };
    let (end, relayed, ()) = tokio::join!(
        session(ours, LAUNCH, &context),
        crate::bridge::relay(theirs, input, stdin_end, out),
        drive
    );
    assert_eq!(end, SessionEnd::Closed);
    assert!(matches!(relayed, Ok(())));
}

/// A success ready before its question's deadline but read after it, with the launch's own
/// deadline still ahead and its gate still passed, never leaves.
#[tokio::test]
async fn a_reply_ready_before_its_questions_deadline_but_read_after_it_never_leaves() {
    let fixture = fixture();
    let (calls, mut brain) = mpsc::channel(1);
    let until = Instant::now() + Duration::from_millis(500);
    let context = calling_until(&fixture, calls, until);
    let end = with_session(&context, |mut peer| async move {
        peer.hello().await;
        peer.request(&key(1), Method::ToolsCall, &call("probe"))
            .await;
        let call = brain.recv().await.expect("admitted before the deadline");
        let probed = McpReply {
            text: "probed".to_owned(),
            error: false,
        };
        call.reply.send(probed).expect("waiting");
        // Blocks the only thread, so the session reads the answer after the deadline.
        std::thread::sleep(until.saturating_duration_since(Instant::now()));
        assert_eq!(peer.reply().await, (key(1), text_result(ENDED_TEXT), false));
    })
    .await;
    assert_eq!(end, SessionEnd::Closed);
}

/// A reply whose write is still blocked at its question's deadline ends the session there, long
/// before the launch's own deadline, with its frame unfinished.
#[tokio::test]
async fn a_reply_write_blocked_past_its_questions_deadline_closes_the_session_unfinished() {
    let fixture = fixture();
    let (calls, mut brain) = mpsc::channel(1);
    let until = Instant::now() + Duration::from_millis(500);
    let context = calling_until(&fixture, calls, until);
    let (ours, theirs) = tokio::io::duplex(1024);
    let drive = async move {
        let mut peer = Peer(theirs);
        peer.hello().await;
        peer.request(&key(1), Method::ToolsCall, &call("probe"))
            .await;
        let call = brain.recv().await.expect("admitted");
        let long = McpReply {
            text: "w".repeat(4096),
            error: false,
        };
        call.reply.send(long).expect("waiting");
        // The bridge reads nothing more until past the question's deadline.
        tokio::time::sleep_until(until + Duration::from_millis(200)).await;
        peer
    };
    let (end, mut peer) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(session(ours, LAUNCH, &context), drive)
    })
    .await
    .expect("ended by the question's deadline, not the launch's");
    assert_eq!(end, SessionEnd::Deadline);
    let mut rest = Vec::new();
    peer.0.read_to_end(&mut rest).await.expect("read");
    let read = crate::ipc::read::<ToBridge>(&mut &rest[..], Pipe::Data).await;
    assert!(read.is_err(), "an unfinished frame: {:?}", read.map(|_| ()));
}

/// A reply whose one frame is blocked when its launch's gate fails ends the session at once,
/// woken by the gate alone, from another thread, before the pipe has room again: the frame stays
/// unfinished.
#[tokio::test]
async fn a_gate_that_fails_while_a_frame_is_blocked_closes_the_session_on_it_unfinished() {
    let fixture = fixture();
    let (calls, mut brain) = mpsc::channel(1);
    let context = calling(&fixture, calls);
    let gate = context.gate.clone();
    let (ours, theirs) = tokio::io::duplex(1024);
    let drive = async move {
        let mut peer = Peer(theirs);
        peer.hello().await;
        peer.request(&key(1), Method::ToolsCall, &call("probe"))
            .await;
        let call = brain.recv().await.expect("admitted");
        let long = McpReply {
            text: "w".repeat(4096),
            error: false,
        };
        call.reply.send(long).expect("waiting");
        // The frame fills the pipe and blocks; nothing is read until the session has ended.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            gate.fail();
        });
        peer
    };
    let started = Instant::now();
    let (end, mut peer) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(session(ours, LAUNCH, &context), drive)
    })
    .await
    .expect("ended by the gate, not the launch's deadline");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "woken by the gate, not by the test's own timeout"
    );
    assert_eq!(end, SessionEnd::Withheld);
    let mut rest = Vec::new();
    peer.0.read_to_end(&mut rest).await.expect("read");
    let read = crate::ipc::read::<ToBridge>(&mut &rest[..], Pipe::Data).await;
    assert!(read.is_err(), "an unfinished frame: {:?}", read.map(|_| ()));
}

/// A call still waiting for its answer ends once its launch's gate fails, long before its
/// deadline, and the model reads that it ended.
#[tokio::test]
async fn a_call_waiting_for_its_answer_ends_when_its_gate_fails() {
    let fixture = fixture();
    let (calls, mut brain) = mpsc::channel(1);
    let context = calling(&fixture, calls);
    let gate = context.gate.clone();
    let session = with_session(&context, |mut peer| async move {
        peer.hello().await;
        peer.request(&key(1), Method::ToolsCall, &call("probe"))
            .await;
        let _unanswered = brain.recv().await.expect("admitted");
        gate.fail();
        assert_eq!(peer.reply().await, (key(1), text_result(ENDED_TEXT), false));
    });
    let end = tokio::time::timeout(Duration::from_secs(10), session)
        .await
        .expect("ended by the gate, not the deadline");
    assert_eq!(end, SessionEnd::Closed);
}

fn probed(chunk: Vec<u8>) -> ToBridge {
    ToBridge::Reply {
        key: key(1),
        chunk,
        last: true,
        error: false,
    }
}

/// A pipe that takes everything, but only once `.0` has elapsed.
struct ReadyAt(Pin<Box<tokio::time::Sleep>>, Vec<u8>);

impl AsyncWrite for ReadyAt {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.0.as_mut().poll(cx));
        this.1.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A pipe that turns writable at the frame's deadline loses to it and takes nothing.
#[tokio::test]
async fn a_pipe_that_turns_writable_at_the_deadline_takes_nothing() {
    let at = Instant::now() + Duration::from_millis(200);
    let mut pipe = ReadyAt(Box::pin(tokio::time::sleep_until(at)), Vec::new());
    let gate = AuditGate::new();
    gate.pass();
    let sent = send(&mut pipe, &probed(b"{}".to_vec()), at, Some(&gate)).await;
    assert_eq!(sent, Err(SessionEnd::Deadline));
    assert_eq!(pipe.1, Vec::<u8>::new());
}

/// A pipe that takes at most 16 bytes a write and fails `.0` after its first write.
struct Trickle(AuditGate, Vec<u8>);

impl AsyncWrite for Trickle {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let taken = buf.len().min(16);
        this.1.extend_from_slice(&buf[..taken]);
        this.0.fail();
        Poll::Ready(Ok(taken))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Every write of a frame checks the gate first, so one that fails between two writes of the
/// same frame stops it there, inside a single poll.
#[tokio::test]
async fn each_write_of_a_frame_checks_the_gate_first() {
    let gate = AuditGate::new();
    gate.pass();
    let mut pipe = Trickle(gate.clone(), Vec::new());
    let deadline = Instant::now() + Duration::from_secs(60);
    let sent = send(&mut pipe, &probed(vec![b'w'; 256]), deadline, Some(&gate)).await;
    assert_eq!(sent, Err(SessionEnd::Withheld));
    assert_eq!(pipe.1.len(), 16, "one write, and none after the failure");
}
