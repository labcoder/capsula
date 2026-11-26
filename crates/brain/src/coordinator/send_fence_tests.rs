//! The live `ChatGPT` send fence end to end: the coordinator's own grant, handed to the
//! production Responses engine and writer the way the brain hands it, against an in-memory peer
//! and a synthetic image. Nothing leaves the process.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use kumochi_protocol::Secret;
use kumochi_protocol::message::{Action, ConnectionKind, ErrorKey, EventId, UserError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::{mpsc, watch};
use zeroize::Zeroizing;

use super::routes::{
    CANT_SEE, built_for, choice, failure, held, jpeg, key, line, pictured, started_input,
    told_cant_see, user, written_settlement,
};
use super::{R, T};
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{Connect, ModelClient};
use crate::context::{Built, ModelRequest, Screen};
use crate::coordinator::{
    Availability, BuildKey, CompactBuilt, ContextBuilt, Coordinator, Effect, Input, JobAsk,
    LeaseId, SendKey, StoreOp, Unbuilt,
};
use crate::engines::responses::{AccessAsk, AccessHandle, Bearer, Responses};
use crate::engines::{self, Answer, Asks, Cancel, SendGate, SendLease, TurnEvent};
use crate::store::Generation;

const CHATGPT: ConnectionKind = ConnectionKind::ChatGpt;
const TEXT: &[u8] = include_bytes!("../../fixtures/responses/text.sse");
/// How the synthetic image starts in a request body.
const IMAGE: &str = "data:image/jpeg;base64,";
const WAIT: Duration = Duration::from_secs(5);

/// What the client's end of a connection did, and whether its writes wait.
#[derive(Default)]
struct Valve {
    polls: usize,
    stalled: bool,
    waker: Option<Waker>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The client's end of one in-memory connection, counting its write polls.
struct Client {
    io: DuplexStream,
    valve: Arc<Mutex<Valve>>,
}

impl AsyncRead for Client {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Client {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        {
            let mut valve = lock(&self.valve);
            valve.polls += 1;
            if valve.stalled {
                valve.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
        }
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// The test's end of one connection, and its client's valve.
struct Peer {
    server: DuplexStream,
    valve: Arc<Mutex<Valve>>,
}

impl Peer {
    fn polls(&self) -> usize {
        lock(&self.valve).polls
    }

    fn release(&self) {
        let mut valve = lock(&self.valve);
        valve.stalled = false;
        if let Some(waker) = valve.waker.take() {
            waker.wake();
        }
    }

    /// Everything still to come from the client, which has closed its end.
    async fn rest(&mut self) -> Vec<u8> {
        let mut rest = Vec::new();
        tokio::time::timeout(WAIT, self.server.read_to_end(&mut rest))
            .await
            .expect("the client closed its end")
            .expect("read");
        rest
    }

    /// The request the client wrote, through its body's end.
    async fn request(&mut self) -> String {
        let mut read = Vec::new();
        let mut chunk = vec![0; 16 * 1024];
        loop {
            let n = tokio::time::timeout(WAIT, self.server.read(&mut chunk))
                .await
                .expect("the request comes")
                .expect("read");
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

    async fn answer(&mut self) {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
            TEXT.len()
        );
        self.server.write_all(head.as_bytes()).await.expect("head");
        self.server.write_all(TEXT).await.expect("body");
    }
}

/// An in-memory connection of this capacity, whose writes wait while it's stalled.
fn pipe((capacity, stalled): (usize, bool)) -> (Client, Peer) {
    let (io, server) = tokio::io::duplex(capacity);
    let valve = Arc::new(Mutex::new(Valve {
        stalled,
        ..Valve::default()
    }));
    (
        Client {
            io,
            valve: Arc::clone(&valve),
        },
        Peer { server, valve },
    )
}

/// Connects once `open` says so, with the next connection's capacity and stall.
struct Pipes {
    open: watch::Receiver<bool>,
    next: Arc<Mutex<(usize, bool)>>,
    peers: mpsc::UnboundedSender<Peer>,
}

impl Connect for Pipes {
    type Io = Client;

    async fn connect(&self) -> io::Result<Client> {
        let mut open = self.open.clone();
        open.wait_for(|open| *open)
            .await
            .map_err(io::Error::other)?;
        let (client, peer) = pipe(*lock(&self.next));
        self.peers.send(peer).map_err(io::Error::other)?;
        Ok(client)
    }
}

/// What the engine's idle connection is when the run starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pool {
    Empty,
    Quiet,
    Stale,
}

/// The coordinator and the production engine, wired as the brain wires them.
struct Rig {
    core: Coordinator,
    engine: Responses<Pipes>,
    asks: Asks,
    inputs_tx: mpsc::UnboundedSender<Input>,
    inputs: mpsc::UnboundedReceiver<Input>,
    events_tx: mpsc::Sender<Input>,
    events: mpsc::Receiver<Input>,
    peers: mpsc::UnboundedReceiver<Peer>,
    open: watch::Sender<bool>,
    next: Arc<Mutex<(usize, bool)>>,
    rounds: Vec<Cancel>,
    builds: Vec<BuildKey>,
    pooled: Option<Peer>,
}

impl Rig {
    /// Run R's held image on `ChatGPT` model m, whose catalog lists `images` as taking
    /// images, built and started on the production engine. Connections open while `open` holds,
    /// which a test may change before its first await, when the engine hasn't yet run.
    fn new(pool: Pool, images: &[&str]) -> Self {
        let (open, open_rx) = watch::channel(true);
        let next = Arc::new(Mutex::new((1 << 20, false)));
        let (peers_tx, peers) = mpsc::unbounded_channel();
        let client = ModelClient::new(Pipes {
            open: open_rx,
            next: Arc::clone(&next),
            peers: peers_tx,
        });
        let pooled = (pool != Pool::Empty).then(|| {
            let (io, peer) = pipe((1 << 20, false));
            client.pool_for_tests(io);
            peer
        });
        let pooled = pooled.filter(|_| pool == Pool::Quiet);
        let (handle, mut access) = AccessHandle::new();
        tokio::spawn(async move {
            while let Some(ask) = access.recv().await {
                if let AccessAsk::Bearer { answer, .. } = ask {
                    answer
                        .send(Ok(Bearer {
                            generation: 1,
                            token: Secret::new(Zeroizing::new("SYNTHETIC".to_owned())),
                        }))
                        .ok();
                }
            }
        });
        let (core, hold) = held(
            vec![Input::Engines(CHATGPT, pictured(7, images))],
            choice(CHATGPT, "m"),
            Some(T),
        );
        let (inputs_tx, inputs) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::channel(64);
        let mut rig = Self {
            core,
            engine: Responses::new(client, Budget::limited(None), handle),
            asks: Asks::default(),
            inputs_tx,
            inputs,
            events_tx,
            events,
            peers,
            open,
            next,
            rounds: Vec::new(),
            builds: Vec::new(),
            pooled,
        };
        let admitted = rig
            .core
            .submit_hold(hold, "words".into(), Some(Screen::Image(jpeg())))
            .expect("admitted");
        rig.apply(&admitted);
        rig.handle(Input::TurnStored { run: R });
        rig.handle(built_for(key(), &choice(CHATGPT, "m")));
        rig
    }

    /// Does what the brain does with each effect the engines take.
    fn apply(&mut self, effects: &[Effect]) {
        for effect in effects {
            match effect {
                Effect::StartEngine { request } => {
                    let gate =
                        SendGate::new(request.key, Arc::clone(&self.asks), self.inputs_tx.clone());
                    self.rounds.push(engines::start(
                        &self.engine,
                        request.clone(),
                        self.events_tx.clone(),
                        gate,
                    ));
                }
                Effect::Grant(lease) => {
                    let picture = self.core.picture_grant(*lease);
                    let granted =
                        SendLease::new(*lease, self.inputs_tx.clone()).with_picture(picture);
                    engines::answer(&self.asks, lease.key, Answer::Grant(granted));
                }
                Effect::Rebuild(key) => engines::answer(&self.asks, *key, Answer::Rebuild),
                Effect::Store(StoreOp::BuildContext(build)) => self.builds.push(build.key),
                _ => {}
            }
        }
    }

    fn handle(&mut self, input: Input) -> Vec<Effect> {
        let effects = self
            .core
            .handle_with_screen_grant(input)
            .expect("not stale");
        self.apply(&effects);
        effects
    }

    /// The next ask or settlement the engine sends, handled.
    async fn settle(&mut self) -> (Input, Vec<Effect>) {
        let input = tokio::time::timeout(WAIT, self.inputs.recv())
            .await
            .expect("in time")
            .expect("an input");
        let effects = self.handle(input.clone());
        (input, effects)
    }

    async fn peer(&mut self) -> Peer {
        tokio::time::timeout(WAIT, self.peers.recv())
            .await
            .expect("in time")
            .expect("a connection")
    }

    /// The round's events through its finish, each handed to the coordinator.
    async fn finish(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        loop {
            let input = tokio::time::timeout(WAIT, self.events.recv())
                .await
                .expect("in time")
                .expect("an event");
            let finished = matches!(
                input,
                Input::Engine {
                    event: TurnEvent::Finished(_),
                    ..
                }
            );
            effects.extend(self.handle(input));
            if finished {
                return effects;
            }
        }
    }

    /// The send's settlement and then the round's terminal, handled in the order the runtime's
    /// biased loop takes them: the settlement must already be queued once the terminal is.
    async fn settled_then_finished(&mut self) -> (Input, Vec<Effect>, Vec<Effect>) {
        let finished = tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("in time")
            .expect("an event");
        assert!(matches!(
            finished,
            Input::Engine {
                event: TurnEvent::Finished(_),
                ..
            }
        ));
        let settled = self
            .inputs
            .try_recv()
            .expect("the settlement is queued ahead of the terminal");
        let effects = self.handle(settled.clone());
        let ended = self.handle(finished);
        (settled, effects, ended)
    }

    /// Run R's image goes on a first send, then the run compacts inline: the rewrite is built and
    /// started, its grant not yet asked for.
    async fn exposed_rewrite(&mut self) -> BuildKey {
        let (_, effects) = self.settle().await;
        let lease = granted(&effects);
        let mut peer = self.peer().await;
        peer.request().await;
        let (settled, effects) = self.settle().await;
        assert_eq!(settled, Input::SendSettled { lease });
        assert_eq!(written_settlement(&effects), (1, 1, 1));
        self.rounds.clear();
        drop(peer);
        let mut effects = Vec::new();
        self.core.build(R, 2, &mut effects);
        self.apply(&effects);
        let round = *self.builds.last().expect("a later round");
        let compact = self.handle(Input::ContextBuilt(ContextBuilt {
            key: round,
            snapshot: None,
            result: Err(Unbuilt::TooLarge { room: Some(500) }),
        }));
        let rewrite = compact
            .iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::CompactContext(context)) => Some(context.key),
                _ => None,
            })
            .expect("it compacts inline");
        self.handle(Input::CompactBuilt(CompactBuilt {
            key: rewrite,
            result: Ok(Some((
                JobAsk {
                    generation: Generation(1),
                    built: Built {
                        recovered_context: None,
                        request: ModelRequest {
                            choice: Some(choice(CHATGPT, "m")),
                            instructions: String::new(),
                            input: vec![user()],
                            tools: Vec::new(),
                        },
                        sources: vec![T],
                    },
                    covers: Some((EventId(1), EventId(2))),
                },
                100,
            ))),
        }));
        rewrite
    }

    /// The fact that m takes images goes and comes back, with the tag and model unchanged.
    fn lose_and_restore(&mut self) {
        for images in [&[][..], &["m"]] {
            self.handle(Input::Engines(CHATGPT, pictured(7, images)));
        }
    }
}

async fn settle_tasks() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

fn granted(effects: &[Effect]) -> LeaseId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Grant(lease) => Some(*lease),
            _ => None,
        })
        .expect("granted")
}

fn sends_again(effects: &[Effect]) -> bool {
    effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::StartEngine { .. } | Effect::Store(StoreOp::BuildContext(_))
        )
    })
}

#[tokio::test]
async fn a_listed_image_goes_once_to_its_exact_model_under_the_coordinators_grant() {
    let mut rig = Rig::new(Pool::Empty, &["m"]);
    let (_, effects) = rig.settle().await;
    let lease = granted(&effects);
    let grant = rig.core.picture_grant(lease).expect("a revocable grant");
    rig.handle(Input::Engines(CHATGPT, pictured(7, &["m", "other"])));
    assert!(
        grant.is_open(),
        "a change that keeps m's fact keeps the grant"
    );
    let mut peer = rig.peer().await;
    let sent = peer.request().await;
    assert!(sent.contains(r#""model":"m""#));
    assert_eq!(sent.matches(IMAGE).count(), 1);
    let (settled, effects) = rig.settle().await;
    assert_eq!(settled, Input::SendSettled { lease });
    assert_eq!(written_settlement(&effects), (1, 1, 1));
    peer.answer().await;
    assert_eq!(failure(&rig.finish().await), None);
}

#[tokio::test]
async fn an_unlisted_model_gets_the_turn_without_the_image_and_no_grant() {
    let mut rig = Rig::new(Pool::Empty, &[]);
    let (_, effects) = rig.settle().await;
    assert!(rig.core.picture_grant(granted(&effects)).is_none());
    let mut peer = rig.peer().await;
    let sent = peer.request().await;
    assert_eq!(sent.matches(IMAGE).count(), 0);
    assert!(sent.contains(CANT_SEE));
    let (_, effects) = rig.settle().await;
    assert_eq!(written_settlement(&effects), (0, 0, 0));
    peer.answer().await;
    assert_eq!(failure(&rig.finish().await), None);
}

/// Lost and back between the grant and the first write, on a new connection, a pooled one or
/// one replacing a stale one: no write poll and no byte of the pictured request, one unsent
/// settlement, and the turn goes again without the image to a successful end.
#[tokio::test]
async fn an_image_lost_and_back_before_its_first_write_never_goes_and_the_turn_goes_without_it() {
    for pool in [Pool::Empty, Pool::Quiet, Pool::Stale] {
        let gated = pool != Pool::Quiet;
        let mut rig = Rig::new(pool, &["m"]);
        rig.open.send_replace(!gated);
        let (_, effects) = rig.settle().await;
        let lease = granted(&effects);
        let grant = rig.core.picture_grant(lease).expect("a revocable grant");
        if gated {
            settle_tasks().await;
        }
        rig.lose_and_restore();
        assert!(!grant.is_open(), "{pool:?}");
        rig.open.send_replace(true);
        let (settled, effects) = rig.settle().await;
        assert_eq!(settled, Input::SendUnsent { lease }, "{pool:?}");
        assert_eq!(told_cant_see(&effects), 1, "{pool:?}");
        let mut first = match rig.pooled.take() {
            Some(pooled) => pooled,
            None => rig.peer().await,
        };
        assert_eq!(first.polls(), 0, "{pool:?}");
        assert_eq!(first.rest().await, Vec::<u8>::new(), "{pool:?}");
        settle_tasks().await;
        assert!(
            rig.events.try_recv().is_err(),
            "the refused round says nothing"
        );
        let again = *rig.builds.last().expect("built again");
        assert_ne!(again, key());
        let started = rig.handle(built_for(again, &choice(CHATGPT, "m")));
        assert_eq!(
            started_input(&started),
            Some(vec![user(), line(CANT_SEE)]),
            "{pool:?}"
        );
        let (_, effects) = rig.settle().await;
        assert!(rig.core.picture_grant(granted(&effects)).is_none());
        let mut second = rig.peer().await;
        let sent = second.request().await;
        assert_eq!(sent.matches(IMAGE).count(), 0, "{pool:?}");
        let (_, effects) = rig.settle().await;
        assert_eq!(written_settlement(&effects), (0, 0, 0));
        second.answer().await;
        let ended = rig.finish().await;
        assert_eq!(failure(&ended), None, "{pool:?}");
    }
}

/// Lost and back after the first write poll was admitted, pending or partial: no further poll,
/// the send settles as possibly written, and the run ends without sending the turn again.
#[tokio::test]
async fn an_image_lost_after_an_admitted_first_write_is_never_replayed_or_called_unsent() {
    for partial in [false, true] {
        let mut rig = Rig::new(Pool::Empty, &["m"]);
        *lock(&rig.next) = if partial {
            (256, false)
        } else {
            (1 << 20, true)
        };
        let (_, effects) = rig.settle().await;
        let lease = granted(&effects);
        let grant = rig.core.picture_grant(lease).expect("a revocable grant");
        let mut peer = rig.peer().await;
        settle_tasks().await;
        let polls = peer.polls();
        assert_eq!(polls, if partial { 2 } else { 1 });
        rig.lose_and_restore();
        assert!(!grant.is_open());
        let mut written = vec![0; 256];
        if partial {
            peer.server.read_exact(&mut written).await.expect("read");
        } else {
            peer.release();
        }
        let (settled, effects, ended) = rig.settled_then_finished().await;
        assert_eq!(settled, Input::SendSettled { lease }, "{partial}");
        assert_eq!(written_settlement(&effects), (1, 1, 1), "{partial}");
        assert_eq!(told_cant_see(&effects), 0);
        assert_eq!(peer.polls(), polls, "no poll after the refusal");
        assert_eq!(peer.rest().await, Vec::<u8>::new());
        assert_eq!(
            failure(&ended),
            Some(
                UserError::new(ErrorKey::ScreenPictureStopped, Vec::new(), Vec::new())
                    .expect("valid")
            ),
            "{partial}"
        );
        assert!(!sends_again(&ended), "{partial}");
        assert_eq!(told_cant_see(&ended), 0);
    }
}

/// A run whose image a send may have written compacts inline under a grant too: lost and back
/// before the rewrite's first write, nothing of it goes and the run stops as the picture
/// stopping; held, it goes, and its settlement isn't another picture.
#[tokio::test]
async fn an_exposed_runs_inline_rewrite_goes_only_while_its_image_may() {
    for lost in [true, false] {
        let mut rig = Rig::new(Pool::Empty, &["m"]);
        let rewrite = rig.exposed_rewrite().await;
        rig.open.send_replace(!lost);
        let (_, effects) = rig.settle().await;
        let lease = granted(&effects);
        assert_eq!(lease.key, SendKey::Build(rewrite));
        let grant = rig
            .core
            .picture_grant(lease)
            .expect("an exposed run's rewrite is under a grant");
        if lost {
            settle_tasks().await;
            rig.lose_and_restore();
            assert!(!grant.is_open());
            rig.open.send_replace(true);
        }
        let mut peer = rig.peer().await;
        if lost {
            let (settled, effects) = rig.settle().await;
            assert_eq!(settled, Input::SendUnsent { lease });
            assert_eq!(
                failure(&effects),
                Some(ErrorKey::ScreenPictureStopped.into())
            );
            assert_eq!(written_settlement(&effects), (0, 0, 0));
            assert_eq!(told_cant_see(&effects), 0);
            assert!(!sends_again(&effects));
            assert_eq!(peer.polls(), 0);
            assert_eq!(peer.rest().await, Vec::<u8>::new());
            continue;
        }
        let sent = peer.request().await;
        assert_eq!(sent.matches(IMAGE).count(), 0);
        let (settled, effects) = rig.settle().await;
        assert_eq!(settled, Input::SendSettled { lease });
        assert_eq!(
            written_settlement(&effects),
            (0, 0, 0),
            "a rewrite isn't another picture"
        );
    }
}

/// Withdrawing the picture or signing out after the grant closes it too, each under its own
/// cause: never as the model being unable to see it.
#[test]
fn withdrawal_or_sign_out_after_a_grant_closes_it_by_its_own_cause() {
    for withdrawn in [true, false] {
        let (mut core, hold) = held(
            vec![Input::Engines(CHATGPT, pictured(7, &["m"]))],
            choice(CHATGPT, "m"),
            Some(T),
        );
        core.submit_hold(hold, "words".into(), Some(Screen::Image(jpeg())))
            .expect("admitted");
        core.handle(Input::TurnStored { run: R }).expect("current");
        core.handle(built_for(key(), &choice(CHATGPT, "m")))
            .expect("current");
        let lease = granted(
            &core
                .handle_with_screen_grant(Input::SendReady {
                    key: SendKey::Build(key()),
                    hold: None,
                })
                .expect("current"),
        );
        let grant = core.picture_grant(lease).expect("a revocable grant");
        let changed = if withdrawn {
            core.handle(Input::ScreenHoldsOff).expect("the setting")
        } else {
            core.handle(Input::Engines(
                CHATGPT,
                Availability::Unavailable(crate::coordinator::availability::sign_in_needed()),
            ))
            .expect("never stale")
        };
        assert!(!grant.is_open(), "{withdrawn}");
        assert_eq!(told_cant_see(&changed), 0);
        let settled = core
            .handle(Input::SendUnsent { lease })
            .expect("a lease outlives its run");
        assert_eq!(told_cant_see(&settled), 0, "{withdrawn}");
        if !withdrawn {
            assert_eq!(
                failure(&changed).map(|error| error.key()),
                Some(ErrorKey::SignInNeeded)
            );
            continue;
        }
        assert!(settled.contains(&Effect::Store(StoreOp::RepairScreen {
            run: R,
            correction: crate::store::UnsentScreen::Withdrawn,
        })));
        let again = settled
            .iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
                _ => None,
            })
            .expect("built again");
        let ended = core
            .handle(built_for(again, &choice(CHATGPT, "m")))
            .expect("current");
        assert_eq!(failure(&ended), Some(ErrorKey::ScreenUnavailable.into()));
    }
}

/// Sharing turned off while an admitted upload is under way: no poll after the refusal, the send
/// settles as possibly written, nothing is sent again, and the run ends as the picture becoming
/// unavailable, not as the connection failing.
#[tokio::test]
async fn sharing_off_mid_upload_stops_its_writes_and_ends_as_the_picture_unavailable() {
    let mut rig = Rig::new(Pool::Empty, &["m"]);
    *lock(&rig.next) = (256, false);
    let (_, effects) = rig.settle().await;
    let lease = granted(&effects);
    let grant = rig.core.picture_grant(lease).expect("a revocable grant");
    let mut peer = rig.peer().await;
    settle_tasks().await;
    let polls = peer.polls();
    assert_eq!(polls, 2);
    rig.handle(Input::ScreenHoldsOff);
    assert!(!grant.is_open());
    let mut written = vec![0; 256];
    peer.server.read_exact(&mut written).await.expect("read");
    let (settled, effects, ended) = rig.settled_then_finished().await;
    assert_eq!(settled, Input::SendSettled { lease });
    assert_eq!(written_settlement(&effects), (1, 1, 1));
    assert_eq!(peer.polls(), polls, "no poll after the refusal");
    assert_eq!(peer.rest().await, Vec::<u8>::new());
    assert_eq!(
        failure(&ended),
        Some(UserError::new(ErrorKey::ScreenUnavailable, Vec::new(), Vec::new()).expect("valid"))
    );
    assert!(!sends_again(&ended));
    assert_eq!(told_cant_see(&ended), 0);
}

/// A connection that fails mid-upload while the grant stays open keeps its own reason.
#[tokio::test]
async fn a_connection_lost_mid_upload_under_an_open_grant_keeps_its_own_reason() {
    let mut rig = Rig::new(Pool::Empty, &["m"]);
    *lock(&rig.next) = (256, false);
    let (_, effects) = rig.settle().await;
    let lease = granted(&effects);
    let grant = rig.core.picture_grant(lease).expect("a revocable grant");
    let peer = rig.peer().await;
    settle_tasks().await;
    assert_eq!(peer.polls(), 2);
    drop(peer);
    let (settled, effects, ended) = rig.settled_then_finished().await;
    assert!(grant.is_open());
    assert_eq!(settled, Input::SendSettled { lease });
    assert_eq!(written_settlement(&effects), (1, 1, 1));
    assert_eq!(
        failure(&ended),
        Some(
            UserError::new(ErrorKey::AccountOffline, Vec::new(), vec![Action::Retry])
                .expect("valid")
        )
    );
    assert!(!sends_again(&ended));
}

fn ended_with(key: ErrorKey) -> UserError {
    UserError::new(key, Vec::new(), Vec::new()).expect("valid")
}

/// Sharing turned off after an exposed run's rewrite admitted a partial write: no later poll,
/// summary, follow-up or second picture mark, and the run ends as the picture unavailable.
#[tokio::test]
async fn sharing_off_after_a_partial_inline_write_ends_as_the_picture_unavailable() {
    let mut rig = Rig::new(Pool::Empty, &["m"]);
    let rewrite = rig.exposed_rewrite().await;
    *lock(&rig.next) = (256, false);
    let (_, effects) = rig.settle().await;
    let lease = granted(&effects);
    assert_eq!(lease.key, SendKey::Build(rewrite));
    let grant = rig
        .core
        .picture_grant(lease)
        .expect("an exposed run's rewrite is under a grant");
    let mut peer = rig.peer().await;
    settle_tasks().await;
    let polls = peer.polls();
    assert_eq!(polls, 2);
    let off = rig.handle(Input::ScreenHoldsOff);
    assert!(!grant.is_open());
    assert_eq!(failure(&off), None);
    let mut written = vec![0; 256];
    peer.server.read_exact(&mut written).await.expect("read");
    let (settled, effects, ended) = rig.settled_then_finished().await;
    assert_eq!(settled, Input::SendSettled { lease });
    assert_eq!(
        written_settlement(&effects),
        (0, 0, 0),
        "no second picture mark"
    );
    assert_eq!(peer.polls(), polls, "no poll after the refusal");
    assert_eq!(peer.rest().await, Vec::<u8>::new());
    assert_eq!(
        failure(&ended),
        Some(ended_with(ErrorKey::ScreenUnavailable))
    );
    assert!(!sends_again(&ended));
    assert!(
        !effects
            .iter()
            .chain(&ended)
            .any(|effect| matches!(effect, Effect::Store(StoreOp::StoreSummary { .. })))
    );
}

/// A capability loss and sharing turned off both close one grant before its send's terminal is
/// read: the run ends by whichever came first.
#[tokio::test]
async fn a_grant_closed_by_two_causes_ends_its_run_by_the_first() {
    for capability_first in [true, false] {
        let mut rig = Rig::new(Pool::Empty, &["m"]);
        *lock(&rig.next) = (1 << 20, true);
        let (_, effects) = rig.settle().await;
        let lease = granted(&effects);
        let grant = rig.core.picture_grant(lease).expect("a revocable grant");
        let peer = rig.peer().await;
        settle_tasks().await;
        assert_eq!(peer.polls(), 1);
        let lost = Input::Engines(CHATGPT, pictured(7, &[]));
        let causes = if capability_first {
            [lost, Input::ScreenHoldsOff]
        } else {
            [Input::ScreenHoldsOff, lost]
        };
        for cause in causes {
            assert_eq!(failure(&rig.handle(cause)), None, "{capability_first}");
        }
        assert!(!grant.is_open());
        peer.release();
        let (settled, effects, ended) = rig.settled_then_finished().await;
        assert_eq!(settled, Input::SendSettled { lease });
        assert_eq!(written_settlement(&effects), (1, 1, 1));
        assert_eq!(peer.polls(), 1, "no poll after the refusal");
        let first = if capability_first {
            ErrorKey::ScreenPictureStopped
        } else {
            ErrorKey::ScreenUnavailable
        };
        assert_eq!(
            failure(&ended),
            Some(ended_with(first)),
            "{capability_first}"
        );
        assert!(!sends_again(&ended));
    }
}

/// A digest-only send (no image) carrying screen-derived metadata runs under a revocable grant on
/// the production Responses transport: losing the recipient's image fact between grant and first
/// write stops every write poll (zero bytes written), settles unsent and rebuilds cleanly without
/// the stale digest; unchanged, the digest-only send writes and finishes.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "seeds a private overlay then checks revoked and open sends"
)]
async fn a_digest_only_send_writes_only_while_its_digest_grant_stays_open() {
    use crate::context::{InputItem, PictureRecipient, Role, Route};
    use crate::drawing::{Input as DrawInput, Reply, Write};
    use crate::engines::CallId;
    use kumochi_protocol::message::{ConnectionKind, ModelSlug, RunId};

    const OVERLAY: &str = r#"{"records":[{"begin":{"v":1,"scene":"private_overlay_sentinel","surface":"overlay","alt":"a"}},{"step":{"id":"s1","put":[{"shape":{"id":"b1","kind":"rect","box":[0,0,10,10]}}]}},{"end":{}}]}"#;
    let recipient = PictureRecipient {
        route: Route {
            kind: ConnectionKind::ChatGpt,
            tag: crate::context::ConnectionTag(7),
        },
        model: ModelSlug::new("m").expect("slug"),
    };

    for revoke in [false, true] {
        let mut rig = Rig::new(Pool::Empty, &["m"]);
        let (_, effects) = rig.settle().await;
        let first_lease = granted(&effects);
        let mut first_peer = rig.peer().await;
        first_peer.request().await;
        let (settled, _) = rig.settle().await;
        assert_eq!(settled, Input::SendSettled { lease: first_lease });
        rig.core.drawing.author(R, Some(&recipient));
        let mut effects = rig.handle(Input::Drawing(DrawInput::Call {
            thread: T,
            run: R,
            call: CallId::new("draw-0").expect("id"),
            args: OVERLAY.into(),
        }));
        while let Some((serial, write)) = effects.iter().find_map(|effect| match effect {
            Effect::Store(StoreOp::Drawing { serial, write }) => Some((*serial, write.clone())),
            _ => None,
        }) {
            let reply = match write {
                Write::Lookup { .. } => Reply::Lookup(None),
                Write::Step { .. } => Reply::Step(super::S),
                Write::Cut { .. } => Reply::Cut,
            };
            effects = rig.handle(Input::DrawingWritten {
                serial,
                reply: Ok(reply),
            });
        }
        first_peer.answer().await;
        assert_eq!(failure(&rig.finish().await), None);
        drop(first_peer);
        rig.handle(Input::RunStored { run: R });
        let digest = rig
            .core
            .drawing
            .digest(T)
            .line_for(&recipient)
            .expect("private overlay digest")
            .to_owned();
        assert!(digest.contains("private_overlay_sentinel"), "{revoke}");

        let next = RunId(R.0 + 1);
        rig.handle(Input::Turn {
            turn: 2,
            thread: Some(T),
            text: "follow-up".into(),
        });
        let stored = rig.handle(Input::TurnStored { run: next });
        let build_key = stored
            .iter()
            .find_map(|effect| match effect {
                Effect::Store(StoreOp::BuildContext(build)) => Some(build.key),
                _ => None,
            })
            .expect("follow-up build");
        let Input::ContextBuilt(mut built) = built_for(build_key, &choice(CHATGPT, "m")) else {
            unreachable!()
        };
        built.snapshot = Some(crate::context::RunSnapshot {
            origin: crate::context::Origin::Task(T),
            assistant: kumochi_protocol::message::AssistantName::new("Kumo").expect("name"),
            user: None,
            choice: Some(choice(CHATGPT, "m")),
            connection: crate::context::ConnectionTag(7),
        });
        built
            .result
            .as_mut()
            .expect("built")
            .request
            .input
            .push(InputItem::Message {
                role: Role::User,
                text: digest,
            });
        if revoke {
            rig.open.send_replace(false);
        }
        rig.handle(Input::ContextBuilt(built));
        let (_, effects) = rig.settle().await;
        let lease = granted(&effects);
        let grant = rig
            .core
            .picture_grant(lease)
            .expect("a digest-only send carries a revocable grant");
        if !revoke {
            let mut peer = rig.peer().await;
            let sent = peer.request().await;
            assert!(sent.contains("private_overlay_sentinel"));
            assert_eq!(sent.matches(IMAGE).count(), 0);
            let (settled, _) = rig.settle().await;
            assert_eq!(settled, Input::SendSettled { lease });
            peer.answer().await;
            assert_eq!(failure(&rig.finish().await), None);
            continue;
        }
        settle_tasks().await;
        rig.handle(Input::Engines(CHATGPT, pictured(7, &[])));
        assert!(!grant.is_open());
        rig.open.send_replace(true);
        let (settled, _) = rig.settle().await;
        assert_eq!(settled, Input::SendUnsent { lease });
        let mut peer = rig.peer().await;
        assert_eq!(
            peer.polls(),
            0,
            "zero transport write polls after revocation"
        );
        assert_eq!(peer.rest().await, Vec::<u8>::new(), "zero bytes written");
        let again = *rig.builds.last().expect("rebuilt cleanly");
        assert_ne!(again, build_key);
        rig.handle(built_for(again, &choice(CHATGPT, "m")));
        let (_, effects) = rig.settle().await;
        let rebuilt_lease = granted(&effects);
        assert!(rig.core.picture_grant(rebuilt_lease).is_none());
        let mut second = rig.peer().await;
        let sent = second.request().await;
        assert!(!sent.contains("private_overlay_sentinel"));
        let (settled, _) = rig.settle().await;
        assert_eq!(
            settled,
            Input::SendSettled {
                lease: rebuilt_lease
            }
        );
        second.answer().await;
        assert_eq!(failure(&rig.finish().await), None);
    }
}
