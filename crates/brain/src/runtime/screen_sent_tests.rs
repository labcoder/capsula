//! A screen hold's turn through the running brain and the real model transport, each way its
//! send can end: the stored mark stays unless the transport proves no write was attempted.

use std::cell::RefCell;
use std::io;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll, Waker};

use kumochi_protocol::Secret;
use kumochi_protocol::message::{ErrorKey, HeldScreen, PageEventKind, PageOf, Phase, SettingsOp};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use zeroize::Zeroizing;

use super::*;
use crate::connections::model::budget::Budget;
use crate::connections::model::conn::{Connect, ModelClient};
use crate::connections::model::{ResponsesBody, stream};
use crate::coordinator::{Effect, HoldKey};
use crate::engines::{Answer, EngineFailure, Finish, TurnEvent};
use crate::runtime::Link;
use crate::runtime::data::DataQueue;
use crate::runtime::settings::Gate;

/// How the scripted connection treats the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wire {
    /// The connection can't be made.
    Refused,
    /// The upload budget is spent before any connection.
    Spent,
    /// Every byte is written and flushed, then the answer is a 500.
    Answered500,
    /// The first write fails: whether it took part of the request below the adapter first, as TLS
    /// can, isn't known.
    WriteFails,
    /// Every byte is written and the flush never finishes, until the user stops the run.
    FlushHeld,
    /// The round attempts a write and keeps its lease until the test lets it go, so the user's
    /// Stop lands before the send settles.
    Kept,
}

struct Scripted {
    wire: Wire,
    /// Hears each attempted write.
    wrote: mpsc::UnboundedSender<()>,
    stall: Option<Arc<Stall>>,
}

impl Connect for Scripted {
    type Io = ScriptedIo;

    fn connect(&self) -> impl std::future::Future<Output = io::Result<ScriptedIo>> + Send {
        let (wire, wrote, stall) = (self.wire, self.wrote.clone(), self.stall.clone());
        async move {
            if let Some(stall) = &stall {
                std::future::poll_fn(|cx| stall.hold(At::Connect, cx)).await;
            }
            if wire == Wire::Refused {
                Err(io::ErrorKind::ConnectionRefused.into())
            } else {
                Ok(ScriptedIo {
                    wire,
                    flushed: false,
                    answered: false,
                    wrote,
                    stall,
                })
            }
        }
    }
}

struct ScriptedIo {
    wire: Wire,
    flushed: bool,
    answered: bool,
    wrote: mpsc::UnboundedSender<()>,
    stall: Option<Arc<Stall>>,
}

/// Where a send waits until the test lets it go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    /// Its connection, before any write poll.
    Connect,
    /// Its first write poll, which is admitted and returns `Pending`.
    Write,
    /// Its flush poll, after every byte was written.
    Flush,
}

/// Holds a send at one point, counting every write and flush poll its connection gets.
struct Stall {
    at: At,
    polls: AtomicUsize,
    reached: mpsc::UnboundedSender<At>,
    gate: std::sync::Mutex<(bool, Option<Waker>)>,
}

impl Stall {
    fn new(at: At) -> (Arc<Self>, mpsc::UnboundedReceiver<At>) {
        let (reached, heard) = mpsc::unbounded_channel();
        let stall = Self {
            at,
            polls: AtomicUsize::new(0),
            reached,
            gate: std::sync::Mutex::new((false, None)),
        };
        (Arc::new(stall), heard)
    }

    fn hold(&self, at: At, cx: &mut Context<'_>) -> Poll<()> {
        let mut gate = self.gate.lock().unwrap();
        if at != self.at || gate.0 {
            return Poll::Ready(());
        }
        if gate.1.is_none() {
            self.reached.send(at).ok();
        }
        gate.1 = Some(cx.waker().clone());
        Poll::Pending
    }

    fn release(&self) {
        let mut gate = self.gate.lock().unwrap();
        gate.0 = true;
        if let Some(waker) = gate.1.take() {
            waker.wake();
        }
    }

    fn polls(&self) -> usize {
        self.polls.load(Ordering::SeqCst)
    }
}

const REFUSAL: &[u8] =
    b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";

impl AsyncRead for ScriptedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.flushed {
            return Poll::Pending;
        }
        if !this.answered {
            this.answered = true;
            buf.put_slice(REFUSAL);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for ScriptedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.wrote.send(()).ok();
        if let Some(stall) = &self.stall {
            stall.polls.fetch_add(1, Ordering::SeqCst);
            if stall.hold(At::Write, cx).is_pending() {
                return Poll::Pending;
            }
        }
        match self.wire {
            Wire::WriteFails => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            _ => Poll::Ready(Ok(buf.len())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(stall) = &this.stall {
            stall.polls.fetch_add(1, Ordering::SeqCst);
            if stall.hold(At::Flush, cx).is_pending() {
                return Poll::Pending;
            }
        }
        if this.wire == Wire::FlushHeld {
            return Poll::Pending;
        }
        this.flushed = true;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A Responses round without its account: the real transport under the coordinator's lease.
struct Transported {
    client: Arc<ModelClient<Scripted>>,
    budget: Arc<Budget>,
}

impl std::fmt::Debug for Transported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Transported")
    }
}

impl crate::engines::Engine for Transported {
    fn start(
        &self,
        _: crate::engines::RunRequest,
        events: mpsc::Sender<TurnEvent>,
        gate: crate::engines::SendGate,
    ) -> crate::engines::Round {
        let (client, budget) = (Arc::clone(&self.client), Arc::clone(&self.budget));
        Box::pin(async move {
            let body = ResponsesBody::new(&json!({"model": "m", "store": false, "stream": true}))
                .expect("a valid body");
            let bearer = Secret::new(Zeroizing::new("bearer".to_owned()));
            let ask = async {
                match gate.ask().await {
                    Answer::Grant(lease) => Some(lease),
                    Answer::Rebuild => None,
                }
            };
            let read = stream(&client, &body, &bearer, ask, &budget, |_: &_| {}).await;
            let finish = read.map_or_else(
                |failure| Finish::Failed(EngineFailure::Stream(failure)),
                |()| Finish::Completed,
            );
            events.send(TurnEvent::Finished(finish)).await.ok();
        })
    }
}

/// A round that takes its grant, says a write was attempted, and hands its lease to the test, so
/// its settlement waits for the test however the round ends.
struct Kept(mpsc::UnboundedSender<crate::engines::SendLease>);

impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kept")
    }
}

impl crate::engines::Engine for Kept {
    fn start(
        &self,
        _: crate::engines::RunRequest,
        events: mpsc::Sender<TurnEvent>,
        gate: crate::engines::SendGate,
    ) -> crate::engines::Round {
        let kept = self.0.clone();
        Box::pin(async move {
            if let Answer::Grant(mut lease) = gate.ask().await {
                crate::connections::model::Lease::wrote(&mut lease);
                kept.send(lease).ok();
            }
            let _open = events;
            std::future::pending::<()>().await;
        })
    }
}

/// The user turn's kind on the first page of `thread`, read as the shell's repair read does.
async fn page_turn(
    shell: &mut Shell<tokio::io::DuplexStream>,
    thread: ThreadId,
) -> Option<PageEventKind> {
    shell.credit(64 * 1024).await;
    let ask = DataToBrain::PageRequest {
        request: 7,
        page: PageOf::Events {
            thread,
            before: None,
        },
    };
    let frame = frame::encode(&ask, Pipe::Data).expect("encodes");
    shell.data_out.write_all(&frame).await.expect("sent");
    loop {
        if let DataToShell::Page {
            request: 7, events, ..
        } = shell.data().await
        {
            return events
                .iter()
                .map(|event| event.kind)
                .find(|kind| matches!(kind, PageEventKind::User | PageEventKind::HeldTurn { .. }));
        }
    }
}

/// After the run's Interrupted state: the history read the shell starts, then the kept send's
/// settlement, told once.
async fn read_then_settle(
    shell: &mut Shell<tokio::io::DuplexStream>,
    lease: crate::engines::SendLease,
    told: &mut Vec<ControlToShell>,
) -> Option<PageEventKind> {
    let thread = shell.thread.expect("the run's thread");
    let repaired = page_turn(shell, thread).await;
    drop(lease);
    loop {
        match shell.control().await {
            Some(message @ ControlToShell::HoldScreen { .. }) => {
                told.push(message);
                break;
            }
            None => break,
            Some(_) => {}
        }
    }
    repaired
}

/// [`play`] without the history read.
async fn played(wire: Wire) -> (Vec<ControlToShell>, Vec<Option<String>>) {
    let (told, screens, _) = Box::pin(play(wire)).await;
    (told, screens)
}

/// Plays a screen hold whose picture is sealed and whose send meets `wire`; the `HoldScreen`s
/// the shell was told, the run's stored screen, and for [`Wire::Kept`] the user turn the history
/// read after Stop showed.
#[expect(
    clippy::too_many_lines,
    reason = "one scripted send through a whole hold"
)]
async fn play(
    wire: Wire,
) -> (
    Vec<ControlToShell>,
    Vec<Option<String>>,
    Option<PageEventKind>,
) {
    use crate::runtime::hold::tests::screen_hold::{recognized, release as released, seeing};
    type Request = <crate::screen::Holds as crate::supervise::Kind>::Request;
    type Reply = <crate::screen::Holds as crate::supervise::Kind>::Reply;
    let scratch = Scratch::new(&format!("hold-screen-sent-{wire:?}"));
    let (mut brain, release, mut decoded, _fetch) =
        input(&scratch, vec![105, -5], "words and a picture").await;
    let mut rig = seeing(&mut brain);
    brain.fixture = Some(crate::coordinator::Ready {
        images: vec![kumochi_protocol::message::ModelSlug::new("gpt-6-sol").unwrap()],
        ..crate::coordinator::Ready::fixture()
    });
    let mut observed = brain.observe();
    let (wrote, mut writes) = mpsc::unbounded_channel();
    let (keep, mut kept) = mpsc::unbounded_channel();
    brain.engine = if wire == Wire::Kept {
        Box::new(Kept(keep))
    } else {
        Box::new(Transported {
            client: Arc::new(ModelClient::new(Scripted {
                wire,
                wrote,
                stall: None,
            })),
            budget: Arc::new(Budget::limited((wire == Wire::Spent).then_some(0))),
        })
    };
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let mut told = Vec::new();
    let mut repaired = None;
    let client = async {
        opened(&mut shell).await;
        shell.send(&recognized(G)).await;
        status(&mut shell, HoldStatus::Admitted).await;
        shell.send(&released(G, 100, 200, 2)).await;
        let (id, display) = rig.next().await;
        crate::screen::hold_frame(&rig, id, display).await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::CaptureDone {
                capture_for: CaptureFor::Gesture(G)
            })
        );
        let (id, seal) = rig.next().await;
        let Request::Seal(key) = seal else {
            panic!("not a seal: {seal:?}")
        };
        let sealed = Reply::Sealed(key, crate::screen::ScreenContext::fixture(PICTURE));
        rig.reply(id, &sealed).await;
        while !matches!(observed.recv().await, Some(Seen::PictureSealed)) {}
        packet(&mut shell, 16_000, 0, true, vec![105, -5]).await;
        release_source(&mut shell, 1).await;
        status(&mut shell, HoldStatus::Transcribing).await;
        assert_eq!(decoded.recv().await, Some(()));
        release.send(()).unwrap();
        let (run, _) = shell.state().await;
        assert!(matches!(shell.data().await, DataToShell::Models { .. }));
        shell.held(G, "words and a picture").await;
        if wire == Wire::FlushHeld {
            assert_eq!(writes.recv().await, Some(()));
            shell.stop(run).await;
        }
        let lease = if wire == Wire::Kept {
            let lease = kept.recv().await.expect("a granted send");
            shell.stop(run).await;
            Some(lease)
        } else {
            None
        };
        shell.read_to_end(run).await;
        loop {
            match shell.control().await {
                Some(message @ ControlToShell::HoldScreen { .. }) => told.push(message),
                Some(ControlToShell::RunState {
                    phase: Phase::Failed | Phase::Interrupted,
                    ..
                })
                | None => break,
                Some(_) => {}
            }
        }
        if let Some(lease) = lease {
            repaired = read_then_settle(&mut shell, lease, &mut told).await;
        }
        drop(shell);
    };
    let (_, ()) = tokio::time::timeout(super::super::PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    brain.close().await;
    let screens = Store::open(&scratch.db())
        .unwrap()
        .conn_for_tests()
        .prepare("SELECT screen FROM runs ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    (told, screens, repaired)
}

/// Stop after a write attempt: the history read that the Interrupted frame starts shows the
/// picture before the send's settlement records it sent, and the settlement tells the hold once.
#[tokio::test]
async fn a_stopped_hold_s_history_shows_its_picture_before_its_send_settles() {
    let told = ControlToShell::HoldScreen {
        gesture: G,
        outcome: HeldScreen::Screen,
    };
    assert_eq!(
        Box::pin(play(Wire::Kept)).await,
        (
            vec![told],
            vec![Some("screen".to_owned())],
            Some(PageEventKind::HeldTurn {
                screen: HeldScreen::Screen
            })
        )
    );
}

#[tokio::test]
async fn a_hold_whose_send_wrote_nothing_stores_and_tells_no_picture() {
    for wire in [Wire::Refused, Wire::Spent] {
        assert_eq!(
            Box::pin(played(wire)).await,
            (Vec::new(), vec![None]),
            "{wire:?}"
        );
    }
}

#[tokio::test]
async fn a_hold_whose_send_may_have_written_keeps_its_picture_and_tells_once() {
    let told = ControlToShell::HoldScreen {
        gesture: G,
        outcome: HeldScreen::Screen,
    };
    for wire in [Wire::Answered500, Wire::WriteFails] {
        assert_eq!(
            Box::pin(played(wire)).await,
            (vec![told.clone()], vec![Some("screen".to_owned())]),
            "{wire:?}"
        );
    }
}

#[tokio::test]
async fn a_hold_stopped_while_its_flush_waits_keeps_its_picture() {
    let (told, screens) = Box::pin(played(Wire::FlushHeld)).await;
    assert!(told.len() <= 1, "{told:?}");
    assert_eq!(screens, [Some("screen".to_owned())]);
}

#[tokio::test]
async fn a_hold_screen_reaches_only_its_hold_s_own_connection() {
    let scratch = Scratch::new("hold-screen-epoch");
    let mut brain = test_support::brain(&scratch, "");
    brain.epoch = 2;
    let (out, mut output) = mpsc::channel(4);
    let queue = RefCell::new(DataQueue::new(0));
    let ready = tokio::sync::Notify::new();
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    for epoch in [1, 3, 2] {
        let hold = HoldKey { epoch, gesture: G };
        let outcome = HeldScreen::Attached;
        assert!(
            brain
                .apply(vec![Effect::HoldScreen { hold, outcome }], Some(&mut link))
                .is_ok()
        );
    }
    let bytes = output.try_recv().unwrap();
    assert_eq!(
        kumochi_protocol::frame::decode::<ControlToShell>(
            &bytes[kumochi_protocol::frame::HEADER_LEN..]
        )
        .unwrap(),
        ControlToShell::HoldScreen {
            gesture: G,
            outcome: HeldScreen::Attached
        }
    );
    assert!(output.try_recv().is_err(), "only the hold's own connection");
    brain.close().await;
}

/// What a send held at `at` came to when show-screen Off arrived while its save was held.
#[derive(Debug, PartialEq)]
struct OffMidSend {
    /// Write and flush polls before Off, and after the send was let go.
    polls: (usize, usize),
    /// The run's failure.
    error: Option<kumochi_protocol::message::UserError>,
    told: Vec<ControlToShell>,
    /// `show_screen` in the answer to Off, then in the answer to a later On.
    answers: (bool, bool),
    /// Polls once On was answered.
    after_on: usize,
    screens: Vec<Option<String>>,
}

/// A screen hold's send held at `at`, then show-screen Off with its save held, then the send let
/// go, then the save finished by `save`, then show-screen On.
#[expect(
    clippy::too_many_lines,
    reason = "one hold, one send and two settings, in order"
)]
async fn off_mid_send(at: At, save: Gate) -> OffMidSend {
    use crate::runtime::hold::tests::screen_hold::{recognized, release as released, seeing};
    type Request = <crate::screen::Holds as crate::supervise::Kind>::Request;
    type Reply = <crate::screen::Holds as crate::supervise::Kind>::Reply;
    let scratch = Scratch::new(&format!(
        "hold-screen-off-{at:?}-{}",
        matches!(save, Gate::Panic)
    ));
    let (mut brain, release, mut decoded, _fetch) =
        input(&scratch, vec![105, -5], "words and a picture").await;
    let mut rig = seeing(&mut brain);
    brain.fixture = Some(crate::coordinator::Ready {
        images: vec![kumochi_protocol::message::ModelSlug::new("gpt-6-sol").unwrap()],
        ..crate::coordinator::Ready::fixture()
    });
    let mut observed = brain.observe();
    let (wrote, _writes) = mpsc::unbounded_channel();
    let (stall, mut reached) = Stall::new(at);
    brain.engine = Box::new(Transported {
        client: Arc::new(ModelClient::new(Scripted {
            wire: Wire::Answered500,
            wrote,
            stall: Some(Arc::clone(&stall)),
        })),
        budget: Arc::new(Budget::limited(None)),
    });
    let saving = brain.hold_config_write();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let mut seen = None;
    let client = async {
        opened(&mut shell).await;
        shell.send(&recognized(G)).await;
        status(&mut shell, HoldStatus::Admitted).await;
        shell.send(&released(G, 100, 200, 2)).await;
        let (id, display) = rig.next().await;
        crate::screen::hold_frame(&rig, id, display).await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::CaptureDone {
                capture_for: CaptureFor::Gesture(G)
            })
        );
        let (id, seal) = rig.next().await;
        let Request::Seal(key) = seal else {
            panic!("not a seal: {seal:?}")
        };
        let sealed = Reply::Sealed(key, crate::screen::ScreenContext::fixture(PICTURE));
        rig.reply(id, &sealed).await;
        while !matches!(observed.recv().await, Some(Seen::PictureSealed)) {}
        packet(&mut shell, 16_000, 0, true, vec![105, -5]).await;
        release_source(&mut shell, 1).await;
        status(&mut shell, HoldStatus::Transcribing).await;
        assert_eq!(decoded.recv().await, Some(()));
        release.send(()).unwrap();
        let (run, _) = shell.state().await;
        assert!(matches!(shell.data().await, DataToShell::Models { .. }));
        shell.held(G, "words and a picture").await;
        assert_eq!(reached.recv().await, Some(at));
        let before = stall.polls();
        shell
            .send(&ControlToBrain::Settings(SettingsOp::SetShowScreen(false)))
            .await;
        // Taken in order on the control pipe, so Off has been taken once this stale Stop has.
        shell.stop(RunId(0)).await;
        while observed.recv().await != Some(Seen::ControlApplied) {}
        stall.release();
        let mut told = Vec::new();
        let error = loop {
            match next_frame(&mut shell).await {
                Some(message @ ControlToShell::HoldScreen { .. }) => told.push(message),
                Some(ControlToShell::RunState {
                    run: of,
                    phase: Phase::Failed | Phase::Interrupted | Phase::Done,
                    error,
                    ..
                }) if of == run => break error,
                Some(other) => assert!(
                    !matches!(other, ControlToShell::Settings { answer: true, .. }),
                    "Off was answered while its save was held"
                ),
                None => panic!("the shell's pipe ended"),
            }
        };
        let after = stall.polls();
        saving.send(save).unwrap();
        let off = show_screen_answer(&mut shell).await;
        shell
            .send(&ControlToBrain::Settings(SettingsOp::SetShowScreen(true)))
            .await;
        let on = show_screen_answer(&mut shell).await;
        seen = Some(OffMidSend {
            polls: (before, after),
            error,
            told,
            answers: (off, on),
            after_on: stall.polls(),
            screens: Vec::new(),
        });
        drop(shell);
    };
    tokio::time::timeout(super::super::PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    brain.close().await;
    let mut seen = seen.expect("the client finished");
    seen.screens = Store::open(&scratch.db())
        .unwrap()
        .conn_for_tests()
        .prepare("SELECT screen FROM runs ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    seen
}

/// The next control frame, settings included, answering pings on the way.
async fn next_frame(shell: &mut Shell<tokio::io::DuplexStream>) -> Option<ControlToShell> {
    loop {
        match tokio::time::timeout(super::super::PATIENCE, shell.next_control())
            .await
            .expect("in time")
            .expect("readable")
        {
            Some(ControlToShell::Ping) => {
                let pong = kumochi_protocol::frame::encode(
                    &ControlToBrain::Pong,
                    kumochi_protocol::frame::Pipe::Control,
                )
                .expect("encodes");
                tokio::io::AsyncWriteExt::write_all(&mut shell.control_out, &pong)
                    .await
                    .expect("sent");
            }
            Some(
                ControlToShell::Account(_)
                | ControlToShell::Connections(_)
                | ControlToShell::ScreenHolds(_),
            ) => {}
            other => return other,
        }
    }
}

/// `show_screen` in the next settings answer.
async fn show_screen_answer(shell: &mut Shell<tokio::io::DuplexStream>) -> bool {
    loop {
        match next_frame(shell).await {
            Some(ControlToShell::Settings { view, answer: true }) => return view.show_screen,
            None => panic!("the shell's pipe ended"),
            Some(_) => {}
        }
    }
}

fn unavailable() -> kumochi_protocol::message::UserError {
    kumochi_protocol::message::UserError::new(ErrorKey::ScreenUnavailable, Vec::new(), Vec::new())
        .expect("valid")
}

/// Off while an admitted write or flush poll is pending and the save is held: the send is cut at
/// its next poll, settles as possibly written and ends as the picture unavailable. A refused save
/// leaves the preference On, and neither it nor a later On gets the send going again.
#[tokio::test]
async fn show_screen_off_cuts_an_admitted_send_before_its_save_finishes() {
    let told = vec![ControlToShell::HoldScreen {
        gesture: G,
        outcome: HeldScreen::Screen,
    }];
    for (at, polls) in [(At::Write, 1), (At::Flush, 2)] {
        for (save, answers) in [(Gate::Panic, (true, true)), (Gate::Write, (false, true))] {
            let refused = matches!(save, Gate::Panic);
            assert_eq!(
                Box::pin(off_mid_send(at, save)).await,
                OffMidSend {
                    polls: (polls, polls),
                    error: Some(unavailable()),
                    told: told.clone(),
                    answers,
                    after_on: polls,
                    screens: vec![Some("screen".to_owned())],
                },
                "{at:?} refused={refused}"
            );
        }
    }
}

/// Off while the send is still connecting and the save is held: no write poll is ever admitted,
/// so the picture is withdrawn unsent and nothing says it was sent.
#[tokio::test]
async fn show_screen_off_before_any_write_leaves_the_picture_unsent() {
    for (save, answers) in [(Gate::Panic, (true, true)), (Gate::Write, (false, true))] {
        let refused = matches!(save, Gate::Panic);
        assert_eq!(
            Box::pin(off_mid_send(At::Connect, save)).await,
            OffMidSend {
                polls: (0, 0),
                error: Some(unavailable()),
                told: Vec::new(),
                answers,
                after_on: 0,
                screens: vec![Some("withdrawn".to_owned())],
            },
            "refused={refused}"
        );
    }
}
