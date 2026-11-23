use std::num::NonZeroU64;

use kumochi_protocol::geometry::{PhysicalPoint, PhysicalRect, ReleasePointer, TypedDisplay};
use kumochi_protocol::message::{
    AudioSession, Effort, EffortSet, LeaseGeneration, ModelInfo, ModelName, ModelSlug, Narration,
    OutputGeneration, RunOrigin, ScreenChoice, TypedScreenDraft, TypedScreenOffer,
    TypedScreenProof, TypedScreenSubmit,
};

use super::*;
use crate::runtime::hold::tests::screen_hold::seeing;
use crate::screen::{Holds, ScreenContext};
use crate::supervise::{Kind, fixture::Rig};

type Request = <Holds as Kind>::Request;
type Reply = <Holds as Kind>::Reply;

const DISPLAY: TypedDisplay = TypedDisplay {
    id: NonZeroU64::MIN,
    bounds: PhysicalRect {
        origin: [0, 0],
        size: [4, 3],
    },
    dpi: 96,
};
const DRAFT: TypedScreenDraft = TypedScreenDraft {
    revision: 3,
    selection: 7,
};
const OFFER: TypedScreenOffer = TypedScreenOffer(NonZeroU64::MIN);
const LEASE: LeaseGeneration = LeaseGeneration(3);

fn desk(scratch: &Scratch) -> (Brain, Rig<Holds>) {
    let mut brain = test_support::brain(scratch, "5000 text late");
    let rig = seeing(&mut brain);
    let slug = ModelSlug::new("gpt-6-sol").unwrap();
    brain.fixture = Some(crate::coordinator::Ready {
        models: vec![ModelInfo {
            slug: slug.clone(),
            display_name: ModelName::new("Fixture model").unwrap(),
            supported_efforts: EffortSet::new([Effort::Low]),
        }],
        images: vec![slug],
        ..crate::coordinator::Ready::fixture()
    });
    (brain, rig)
}

async fn offer(shell: &mut Shell<DuplexStream>, turn: u32, expected: Option<TypedScreenOffer>) {
    shell
        .send(&ControlToBrain::RequestTypedScreenOffer {
            turn,
            thread: None,
            draft: DRAFT,
            display: DISPLAY,
        })
        .await;
    assert_eq!(
        shell.control().await,
        Some(ControlToShell::TypedScreenOffered {
            turn,
            thread: None,
            draft: DRAFT,
            offer: expected
        })
    );
}

fn screen(offer: TypedScreenOffer) -> ScreenChoice {
    ScreenChoice::Typed(TypedScreenSubmit {
        offer,
        draft: DRAFT,
        lease: LEASE,
        display: DISPLAY,
        point: ReleasePointer(PhysicalPoint([2, 1])),
        submitted_qpc: 10,
        excluded_qpc: 11,
    })
}

async fn submit(shell: &mut Shell<DuplexStream>, turn: u32, screen: ScreenChoice) {
    let message = DataToBrain::TextSubmit {
        turn,
        thread: None,
        text: "typed words".into(),
        screen,
    };
    let bytes = frame::encode(&message, Pipe::Data).unwrap();
    shell.data_out.write_all(&bytes).await.unwrap();
}

async fn paired(brain: &mut Brain, client: impl AsyncFnOnce(Shell<DuplexStream>)) {
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(shell.scenes().await, []);
        let output = OutputGeneration(7);
        shell
            .send(&ControlToBrain::QueuedAudioOutput(
                24_000,
                output,
                Narration::Plain,
            ))
            .await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::QueuedAudioRegistered(output))
        );
        client(shell).await;
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(
            brain.serve_with_audio(control, data, &mut tap, AudioSession::QueuedRequired),
            client
        )
    })
    .await
    .expect("the pair settles");
    assert_eq!(ended, Ended::Lost);
}

#[tokio::test]
async fn dormant_typed_screen_refuses_in_order_without_touching_the_worker() {
    let scratch = Scratch::new("dormant-typed-screen");
    let (mut brain, mut rig) = desk(&scratch);
    paired(&mut brain, async |mut shell| {
        offer(&mut shell, 1, None).await;
        submit(&mut shell, 1, screen(OFFER)).await;
        submit(&mut shell, 2, ScreenChoice::None).await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::TurnRefused {
                turn: 1,
                thread: None
            })
        );
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::RunState {
                run: RunId(1),
                thread: ThreadId(2),
                phase: Phase::Thinking,
                error: None,
                origin: Some(RunOrigin::Typed { turn: 2 }),
            })
        );
        let client = crate::screen::HoldClient::new(rig.handle.clone());
        let ping = client.ping(tokio::time::Instant::now() + PATIENCE);
        tokio::join!(
            async {
                ping.await.unwrap();
            },
            async {
                let (id, request) = rig.next().await;
                assert!(matches!(request, Request::Ping));
                rig.reply(id, &Reply::Pong).await;
            }
        );
    })
    .await;
    brain.close().await;
}

#[tokio::test]
async fn typed_screen_cancel_overtakes_data_without_starting_capture_or_retargeting() {
    let scratch = Scratch::new("cancel-first-typed-screen");
    let (mut brain, mut rig) = desk(&scratch);
    brain.typed_screen_clock = Some(|stamp| (stamp < 10_000).then(|| Duration::from_millis(stamp)));
    paired(&mut brain, async |mut shell| {
        offer(&mut shell, 1, Some(OFFER)).await;
        shell
            .send(&ControlToBrain::CancelTypedScreen {
                turn: 1,
                offer: OFFER,
            })
            .await;
        offer(&mut shell, 19, None).await;
        submit(&mut shell, 1, screen(OFFER)).await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::TurnRefused {
                turn: 1,
                thread: None
            })
        );
        let next = TypedScreenOffer(NonZeroU64::new(2).unwrap());
        offer(&mut shell, 2, Some(next)).await;
        shell
            .send(&ControlToBrain::CancelTypedScreen {
                turn: 1,
                offer: OFFER,
            })
            .await;
        submit(&mut shell, 2, screen(next)).await;
        let (id, request) = rig.next().await;
        crate::screen::hold_frame(&rig, id, request).await;
        let Some(ControlToShell::TypedScreenHeld(proof)) = shell.control().await else {
            panic!("no held frame for the fresh offer");
        };
        assert_eq!(
            proof,
            TypedScreenProof {
                turn: 2,
                offer: next,
                lease: LEASE,
                capture: NonZeroU64::MIN
            }
        );
        shell
            .send(&ControlToBrain::ConfirmTypedScreen(TypedScreenProof {
                capture: NonZeroU64::new(2).unwrap(),
                ..proof
            }))
            .await;
        shell.send(&ControlToBrain::ConfirmTypedScreen(proof)).await;
        let (id, request) = rig.next().await;
        let Request::Seal(key) = request else {
            panic!("the proven capture did not seal")
        };
        rig.reply(
            id,
            &Reply::Sealed(key, ScreenContext::fixture(b"image sentinel")),
        )
        .await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::RunState {
                run: RunId(1),
                thread: ThreadId(2),
                phase: Phase::Thinking,
                error: None,
                origin: Some(RunOrigin::Typed { turn: 2 }),
            })
        );
    })
    .await;
    brain.close().await;
}
