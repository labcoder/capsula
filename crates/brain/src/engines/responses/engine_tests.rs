//! The Responses engine offline, on in-memory pipes: the checked body comes before the bearer,
//! the bearer before the ask, a Rebuild sends nothing, and a gone pump stops the stream.

use std::io;
use std::sync::Arc;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{
    Action, Effort, ErrorKey, ModelChoice, ModelSlug, RunId, ThreadId, UserError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use super::{Access, AccessAsk, AccessHandle, Bearer, Responses};
use crate::connections::account::Denied;
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{Connect, ModelClient};
use crate::connections::model::error::{Failure, ProviderError};
use crate::coordinator::{Input, LeaseId, SendKey};
use crate::engines::{
    Answer, Asks, Engine, EngineFailure, Finish, RunRequest, SendGate, SendLease, TurnEvent,
};

const TEXT: &[u8] = include_bytes!("../../../fixtures/responses/text.sse");
const DETAIL: &[u8] = include_bytes!("../../../fixtures/responses/detail.json");
const BEARER: &str = "PLANTED-BEARER";

/// Hands the server end of each connection to the test.
struct Pipes(mpsc::UnboundedSender<DuplexStream>);

impl Connect for Pipes {
    type Io = DuplexStream;

    fn connect(&self) -> impl Future<Output = io::Result<DuplexStream>> + Send {
        let (io, server) = tokio::io::duplex(1 << 16);
        std::future::ready(self.0.send(server).map(|()| io).map_err(io::Error::other))
    }
}

/// One round on its way, and every side of it the test plays.
struct Round {
    events: mpsc::Receiver<TurnEvent>,
    access: mpsc::UnboundedReceiver<AccessAsk>,
    inputs: mpsc::UnboundedReceiver<Input>,
    inputs_tx: mpsc::UnboundedSender<Input>,
    asks: Asks,
    servers: mpsc::UnboundedReceiver<DuplexStream>,
    done: JoinHandle<()>,
}

fn request(model: Option<&str>) -> RunRequest {
    let mut request = RunRequest::for_tests(RunId(7), ThreadId(2), "secret words");
    request.request.choice = model.map(|slug| ModelChoice {
        model: ModelSlug::new(slug).expect("slug"),
        effort: Effort::Low,
    });
    request
}

fn key() -> SendKey {
    request(None).key
}

/// The generation [`bearer`] comes from.
const GENERATION: u64 = 7;

fn bearer() -> Bearer {
    Bearer {
        generation: GENERATION,
        token: Secret::new(Zeroizing::new(BEARER.to_owned())),
    }
}

fn start(request: RunRequest) -> Round {
    let (servers_tx, servers) = mpsc::unbounded_channel();
    let (handle, access) = AccessHandle::new();
    let engine = Responses::new(
        ModelClient::new(Pipes(servers_tx)),
        Budget::limited(None),
        handle,
    );
    let (inputs_tx, inputs) = mpsc::unbounded_channel();
    let asks = Asks::default();
    let gate = SendGate::new(request.key, Arc::clone(&asks), inputs_tx.clone());
    let (events_tx, events) = mpsc::channel(8);
    let done = tokio::spawn(engine.start(request, events_tx, gate));
    Round {
        events,
        access,
        inputs,
        inputs_tx,
        asks,
        servers,
        done,
    }
}

impl Round {
    async fn bearer(&mut self, access: Access) {
        let ask = self
            .access
            .recv()
            .await
            .expect("the round asks for its bearer");
        let AccessAsk::Bearer { key: asked, answer } = ask else {
            panic!("a bearer ask first, not {ask:?}");
        };
        assert_eq!(asked, key());
        answer.send(access).ok();
    }

    fn lease(&self) -> Answer {
        Answer::Grant(SendLease::new(
            LeaseId {
                key: key(),
                send: 1,
            },
            self.inputs_tx.clone(),
        ))
    }

    async fn answer(&mut self, answered: Answer) {
        assert_eq!(
            self.inputs.recv().await,
            Some(Input::SendReady { key: key() })
        );
        let (_, answer) = self
            .asks
            .lock()
            .expect("asks")
            .pop()
            .expect("the round's ask");
        answer.send(answered).ok();
    }

    /// Every event the round sends, once it has ended.
    async fn ended(&mut self) -> Vec<TurnEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.events.recv().await {
            events.push(event);
        }
        (&mut self.done).await.expect("the round ends");
        events
    }
}

/// The request the server read, its body's end included.
async fn read_request(server: &mut DuplexStream) -> String {
    let mut read = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let n = server.read(&mut chunk).await.expect("read");
        assert_ne!(n, 0, "the request ended early");
        read.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&read).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .expect("a length")
                .parse()
                .expect("a number");
            if body.len() >= length {
                return text;
            }
        }
    }
}

fn head(length: usize) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\n\r\n"
    )
}

#[tokio::test]
async fn a_refused_body_asks_for_no_bearer() {
    let mut round = start(request(None));
    assert_eq!(
        round.ended().await,
        [TurnEvent::Finished(Finish::Failed(EngineFailure::Stream(
            Failure::Request
        )))]
    );
    assert!(round.access.try_recv().is_err(), "no bearer was asked for");
    assert!(round.inputs.try_recv().is_err(), "no send was asked for");
}

/// M159-1: no bearer fails the round before its ask, carrying the account's own reason when it
/// gave one, such as a refresh that couldn't connect or a save the store refused.
#[tokio::test]
async fn no_bearer_fails_the_round_before_its_ask() {
    let reason = |key| UserError::new(key, Vec::new(), vec![Action::Retry]).expect("one action");
    for (denied, carried) in [
        (Denied::Unusable, None),
        (
            Denied::Failed(reason(ErrorKey::AccountOffline)),
            Some(reason(ErrorKey::AccountOffline)),
        ),
        (
            Denied::Failed(reason(ErrorKey::CredentialsUnavailable)),
            Some(reason(ErrorKey::CredentialsUnavailable)),
        ),
    ] {
        let mut round = start(request(Some("gpt-fixture")));
        round.bearer(Err(denied)).await;
        assert_eq!(
            round.ended().await,
            [TurnEvent::Finished(Finish::Failed(EngineFailure::Access(
                carried.clone()
            )))]
        );
        assert!(round.inputs.try_recv().is_err(), "no send was asked for");
        assert!(round.servers.try_recv().is_err(), "nothing connected");
    }
}

#[tokio::test]
async fn a_rebuild_sends_nothing_and_says_nothing() {
    let mut round = start(request(Some("gpt-fixture")));
    round.bearer(Ok(bearer())).await;
    round.answer(Answer::Rebuild).await;
    assert_eq!(round.ended().await, []);
    assert!(round.servers.try_recv().is_err(), "nothing connected");
}

#[tokio::test]
async fn a_granted_round_sends_on_its_bearer_and_streams_its_answer() {
    let mut round = start(request(Some("gpt-fixture")));
    round.bearer(Ok(bearer())).await;
    let lease = round.lease();
    round.answer(lease).await;
    let mut server = round.servers.recv().await.expect("a connection");
    let sent = read_request(&mut server).await;
    assert!(sent.contains("\r\nAuthorization: Bearer PLANTED-BEARER\r\n"));
    assert!(sent.contains(r#""model":"gpt-fixture""#));
    assert_eq!(
        round.inputs.recv().await,
        Some(Input::SendSettled {
            lease: LeaseId {
                key: key(),
                send: 1
            }
        }),
        "the lease settles once the body is out"
    );
    server
        .write_all(head(TEXT.len()).as_bytes())
        .await
        .expect("head");
    server.write_all(TEXT).await.expect("body");
    assert_eq!(
        round.ended().await,
        [
            TurnEvent::TextDelta("Hello, ".to_owned()),
            TurnEvent::TextDelta("w\u{f6}rld \u{1f642}".to_owned()),
            TurnEvent::Finished(Finish::Completed),
        ]
    );
}

#[tokio::test]
async fn a_gone_pump_stops_the_stream_and_closes_its_connection() {
    let mut round = start(request(Some("gpt-fixture")));
    round.bearer(Ok(bearer())).await;
    let lease = round.lease();
    round.answer(lease).await;
    let mut server = round.servers.recv().await.expect("a connection");
    read_request(&mut server).await;
    round.events.close();
    let end = |from: usize| {
        from + TEXT[from..]
            .windows(2)
            .position(|pair| pair == b"\n\n")
            .expect("an event")
            + 2
    };
    let second = end(end(0));
    server
        .write_all(head(TEXT.len()).as_bytes())
        .await
        .expect("head");
    server.write_all(&TEXT[..second]).await.expect("text");
    // The idle timeout is 60 s, so ending within 5 s is the gone pump's doing.
    tokio::time::timeout(std::time::Duration::from_secs(5), &mut round.done)
        .await
        .expect("closed well before the idle timeout")
        .expect("the round ends");
    let mut rest = [0; 16];
    assert_eq!(
        server.read(&mut rest).await.expect("read"),
        0,
        "the client closed its end with the stream unread"
    );
}

/// OSC-223: a 401 tells the account its bearer's generation was refused, so the next access
/// refreshes it; another refusal, here a 403, tells it nothing.
#[tokio::test]
async fn a_refused_bearer_is_reported_with_its_generation() {
    for (status, refused) in [(401, Some(GENERATION)), (403, None)] {
        let mut round = start(request(Some("gpt-fixture")));
        round.bearer(Ok(bearer())).await;
        let lease = round.lease();
        round.answer(lease).await;
        let mut server = round.servers.recv().await.expect("a connection");
        read_request(&mut server).await;
        let head = format!(
            "HTTP/1.1 {status} Refused\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            DETAIL.len()
        );
        server.write_all(head.as_bytes()).await.expect("head");
        server.write_all(DETAIL).await.expect("body");
        let failure = Failure::Provider(ProviderError {
            status,
            code: None,
            param: None,
        });
        assert_eq!(
            round.ended().await,
            [TurnEvent::Finished(Finish::Failed(EngineFailure::Stream(
                failure
            )))],
            "{status}"
        );
        let told = round.access.try_recv().ok().map(|ask| match ask {
            AccessAsk::Refused(generation) => generation,
            AccessAsk::Bearer { .. } => panic!("no second bearer ask"),
        });
        assert_eq!(told, refused, "{status}");
    }
}

/// The bearer never prints through the access answer or the ask that carries it.
#[test]
fn formatting_an_access_answer_or_ask_never_prints_the_bearer() {
    let (answer, _answered) = tokio::sync::oneshot::channel();
    let access: Access = Ok(bearer());
    let ask = AccessAsk::Bearer { key: key(), answer };
    let byte_list = format!("{:?}", BEARER.as_bytes());
    let byte_list = byte_list.trim_matches(['[', ']']);
    for shown in [format!("{access:?}"), format!("{ask:?}")] {
        assert!(!shown.contains(BEARER), "the bearer leaked: {shown}");
        assert!(!shown.contains(byte_list), "the bearer's bytes leaked");
    }
}
