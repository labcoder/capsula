//! A screen hold's turn through the running brain and the real model transport, each way its
//! send can end: the stored mark stays unless the transport proves no write was attempted.

use std::cell::RefCell;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use kumochi_protocol::Secret;
use kumochi_protocol::message::{HeldScreen, Phase};
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
}

struct Scripted {
    wire: Wire,
    /// Hears each attempted write.
    wrote: mpsc::UnboundedSender<()>,
}

impl Connect for Scripted {
    type Io = ScriptedIo;

    fn connect(&self) -> impl std::future::Future<Output = io::Result<ScriptedIo>> + Send {
        std::future::ready(if self.wire == Wire::Refused {
            Err(io::ErrorKind::ConnectionRefused.into())
        } else {
            Ok(ScriptedIo {
                wire: self.wire,
                flushed: false,
                answered: false,
                wrote: self.wrote.clone(),
            })
        })
    }
}

struct ScriptedIo {
    wire: Wire,
    flushed: bool,
    answered: bool,
    wrote: mpsc::UnboundedSender<()>,
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
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.wrote.send(()).ok();
        match self.wire {
            Wire::WriteFails => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            _ => Poll::Ready(Ok(buf.len())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
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

/// Plays a screen hold whose picture is sealed and whose send meets `wire`; the `HoldScreen`s
/// the shell was told and the run's stored screen.
async fn played(wire: Wire) -> (Vec<ControlToShell>, Vec<Option<String>>) {
    use crate::runtime::hold::tests::screen_hold::{recognized, release as released, seeing};
    type Request = <crate::screen::Holds as crate::supervise::Kind>::Request;
    type Reply = <crate::screen::Holds as crate::supervise::Kind>::Reply;
    let scratch = Scratch::new(&format!("hold-screen-sent-{wire:?}"));
    let (mut brain, release, mut decoded, _fetch) =
        input(&scratch, vec![105, -5], "words and a picture").await;
    let mut rig = seeing(&mut brain);
    let mut observed = brain.observe();
    let (wrote, mut writes) = mpsc::unbounded_channel();
    brain.engine = Box::new(Transported {
        client: Arc::new(ModelClient::new(Scripted { wire, wrote })),
        budget: Arc::new(Budget::limited((wire == Wire::Spent).then_some(0))),
    });
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let mut told = Vec::new();
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
    (told, screens)
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
