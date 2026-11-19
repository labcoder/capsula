use std::io;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{DataToBrain, RunId, ScreenChoice, ThreadId};
use tokio::io::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream,
};

use super::*;
use crate::runtime::tests::{outcome, outcomes};

const TEXT: &str = include_str!("../../../fixtures/responses/text.sse");

struct ResponsesReplay(mpsc::UnboundedSender<DuplexStream>);

impl Connect for ResponsesReplay {
    type Io = DuplexStream;

    fn connect(&self) -> impl Future<Output = io::Result<Self::Io>> + Send {
        let (client, server) = tokio::io::duplex(1 << 16);
        std::future::ready(
            self.0
                .send(server)
                .map(|()| client)
                .map_err(io::Error::other),
        )
    }
}

struct RefreshReplay(SharedHttp);

impl Http for RefreshReplay {
    async fn execute(&self, request: Request) -> Result<Response, HttpError> {
        if matches!(request.endpoint, Endpoint::Token) {
            self.0.execute(request).await
        } else {
            Renewing(|| Err(HttpError::Configuration))
                .execute(request)
                .await
        }
    }
}

fn replay_brain(
    scratch: &Scratch,
    accounts: Accounts,
) -> (crate::runtime::Brain, mpsc::UnboundedReceiver<DuplexStream>) {
    let (send, receive) = mpsc::unbounded_channel();
    let (access, asks) = AccessHandle::new();
    let engine = Responses::new(
        ModelClient::new(ResponsesReplay(send)),
        Budget::limited(None),
        access,
    );
    let selected = crate::runtime::Selected::Account {
        engine: Box::new(engine),
        asks,
    };
    (test_support::brain_on(scratch, selected, accounts), receive)
}

async fn ready(shell: &mut Shell<DuplexStream>) {
    assert!(matches!(
        shell.data().await,
        DataToShell::Models {
            connection: ConnectionKind::ChatGpt,
            ..
        }
    ));
    loop {
        match shell.frame().await {
            ControlToShell::Settings { view, .. } if view.default_model.is_some() => return,
            ControlToShell::Ping => shell.send(&ControlToBrain::Pong).await,
            _ => {}
        }
    }
}

async fn submit(shell: &mut Shell<DuplexStream>, turn: u32, prompt: &str) {
    let message = DataToBrain::TextSubmit {
        turn,
        screen: ScreenChoice::None,
        thread: Some(ThreadId(1)),
        text: prompt.into(),
    };
    let bytes = frame::encode(&message, Pipe::Data).expect("typed submit");
    shell.data_out.write_all(&bytes).await.expect("sent");
}

async fn request(socket: DuplexStream, bearer: &str, prompt: &str) -> DuplexStream {
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    reader.read_line(&mut line).await.expect("request line");
    assert_eq!(line, "POST /v1/responses HTTP/1.1\r\n");
    let (mut length, mut authorized) = (None, false);
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).await.expect("header"), 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = Some(value.trim().parse::<usize>().expect("body length"));
        }
        if let Some(value) = line.strip_prefix("Authorization: Bearer ") {
            authorized = value.trim_end() == bearer;
        }
    }
    assert!(authorized, "request used the expected fixture bearer");
    let mut body = vec![0; length.expect("content length")];
    reader.read_exact(&mut body).await.expect("request body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("request JSON");
    let current = body["input"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|item| item["role"] == "user")
        .expect("user message");
    assert_eq!(
        current["content"].as_str().expect("text").lines().next(),
        Some(prompt)
    );
    reader.into_inner()
}

async fn head(socket: &mut DuplexStream) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
        TEXT.len()
    );
    socket
        .write_all(header.as_bytes())
        .await
        .expect("response header");
}

async fn complete(shell: &mut Shell<DuplexStream>, run: RunId, mut socket: DuplexStream) {
    head(&mut socket).await;
    socket
        .write_all(TEXT.as_bytes())
        .await
        .expect("recorded response");
    let mut text = String::new();
    loop {
        match shell.data().await {
            DataToShell::AssistantText { run: owner, delta } => {
                assert_eq!(owner, run, "no text from the interrupted run");
                text.push_str(delta.as_str());
                shell.credit(delta.as_str().len() + 64).await;
            }
            DataToShell::RunEnd { run: owner } => {
                assert_eq!(owner, run, "only the new run ends here");
                break;
            }
            other => panic!("unexpected replay frame: {other:?}"),
        }
    }
    assert_eq!(text, "Hello, w\u{f6}rld \u{1f642}");
    assert_eq!(shell.state().await, (run, Phase::Answering));
    assert_eq!(shell.notice().await, (run, Phase::Done, None));
}

#[tokio::test]
async fn typed_replay_esc_stop_and_replacement_close_the_old_stream() {
    for reason in ["stop", "replaced"] {
        let scratch = Scratch::new(&format!("responses-typed-{reason}"));
        let store = FakeStore::default();
        let accounts = seeded(&scratch, &store, Catalog::answering([listing()]), LATER);
        let (mut brain, mut connections) = replay_brain(&scratch, accounts);
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            ready(&mut shell).await;
            submit(&mut shell, 1, "first").await;
            let old = RunId(1);
            assert_eq!(shell.state().await, (old, Phase::Thinking));
            let mut first = request(
                connections.recv().await.expect("first HTTP request"),
                "access-old",
                "first",
            )
            .await;
            let split = TEXT
                .match_indices("event: response.output_text.delta")
                .nth(1)
                .expect("second delta")
                .0;
            head(&mut first).await;
            first
                .write_all(&TEXT.as_bytes()[..split])
                .await
                .expect("first delta");
            assert_eq!(
                shell.data().await,
                DataToShell::AssistantText {
                    run: old,
                    delta: "Hello, ".into()
                }
            );
            assert_eq!(shell.state().await, (old, Phase::Answering));
            if reason == "stop" {
                shell.stop(old).await;
            } else {
                submit(&mut shell, 2, "next").await;
            }
            assert_eq!(shell.notice().await, (old, Phase::Interrupted, None));
            assert_eq!(shell.data().await, DataToShell::RunEnd { run: old });
            assert_eq!(
                first.read(&mut [0; 1]).await.expect("provider socket"),
                0,
                "old request closed with a tail still owed"
            );
            assert_eq!(
                first
                    .write_all(&TEXT.as_bytes()[split..])
                    .await
                    .expect_err("the old tail cannot arrive")
                    .kind(),
                io::ErrorKind::BrokenPipe
            );
            if reason == "stop" {
                submit(&mut shell, 2, "next").await;
            }
            let next = RunId(2);
            assert_eq!(shell.state().await, (next, Phase::Thinking));
            let second = request(
                connections.recv().await.expect("next HTTP request"),
                "access-old",
                "next",
            )
            .await;
            complete(&mut shell, next, second).await;
            drop(shell);
        };
        let (ended, ()) = timeout(PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .expect("bounded replay");
        assert_eq!(ended, Ended::Lost);
        brain.close().await;
        assert_eq!(
            outcomes(&scratch),
            [outcome("interrupted", reason), ("done".into(), None)]
        );
    }
}

#[tokio::test]
async fn typed_replay_stop_during_refresh_does_not_send_the_stopped_turn() {
    let scratch = Scratch::new("responses-typed-refresh-stop");
    let store = FakeStore::default();
    let (refresh, sent, release) = gated();
    let http = SharedHttp::new(RefreshReplay(refresh));
    let now = Arc::new(AtomicU64::new(1_800_000_000));
    let accounts = seeded_at(
        &scratch,
        &store,
        Arc::new(move || Ok(http.clone())),
        LATER,
        Arc::new(Ticking(Arc::clone(&now))),
    );
    let (mut brain, mut connections) = replay_brain(&scratch, accounts);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        ready(&mut shell).await;
        now.store(LATER, Ordering::SeqCst);
        submit(&mut shell, 1, "first").await;
        let old = RunId(1);
        assert_eq!(shell.state().await, (old, Phase::Thinking));
        sent.await.expect("refresh in flight");
        shell.stop(old).await;
        assert_eq!(shell.notice().await, (old, Phase::Interrupted, None));
        assert_eq!(shell.data().await, DataToShell::RunEnd { run: old });
        release.send(()).expect("refresh survives Stop");
        submit(&mut shell, 2, "next").await;
        let next = RunId(2);
        assert_eq!(shell.state().await, (next, Phase::Thinking));
        let second = request(
            connections
                .recv()
                .await
                .expect("only the successor reaches the model"),
            "access-new",
            "next",
        )
        .await;
        complete(&mut shell, next, second).await;
        assert_eq!(
            connections.try_recv().err(),
            Some(mpsc::error::TryRecvError::Empty)
        );
        drop(shell);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("bounded replay");
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(
        outcomes(&scratch),
        [outcome("interrupted", "stop"), ("done".into(), None)]
    );
    assert_eq!(
        saved_refresh(&scratch, &store),
        ("refresh-new".into(), false)
    );
}
