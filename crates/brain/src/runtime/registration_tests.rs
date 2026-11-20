use std::cell::{Cell, RefCell};

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::geometry::PhysicalPoint;
use kumochi_protocol::message::{
    AudioSession, ControlToBrain, ControlToShell, DataToBrain, GestureId, LeaseGeneration,
    MAX_RATE, MIN_RATE, NoticeRevision, OutputGeneration, SceneId, ScreenChoice,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

use super::super::{Seen, controls::Registration};
use super::{Brain, DataQueue, Ended, Link, PATIENCE, Shell, duplex, outcomes, window};
use crate::coordinator::Input;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};

const OUTPUT: OutputGeneration = OutputGeneration(7);
const REGISTER: ControlToBrain = ControlToBrain::QueuedAudioOutput(24_000, OUTPUT);
const RECEIPT: ControlToShell = ControlToShell::QueuedAudioRegistered(OUTPUT);

async fn queued(brain: &mut Brain, client: impl AsyncFnOnce(Shell<DuplexStream>)) -> Ended {
    let ((control, data), shell) = duplex();
    let mut tap = Tap::from_env();
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(
            brain.serve_with_audio(control, data, &mut tap, AudioSession::QueuedRequired),
            client(shell)
        )
    })
    .await
    .expect("both pipes and their cleanup finished");
    ended
}

async fn closed(shell: &mut Shell<DuplexStream>) {
    assert_eq!(shell.control().await, None);
    assert_eq!(shell.data_in.read_to_end(&mut Vec::new()).await.unwrap(), 0);
}

#[tokio::test]
async fn early_turns_close_both_pipes_without_replaying_after_a_late_register() {
    let scratch = Scratch::new("early-queued-turn");
    let mut brain = test_support::brain(&scratch, "0 text answer");
    let mut seen = brain.observe();
    for message in [
        DataToBrain::TextSubmit {
            turn: 1,
            thread: None,
            screen: ScreenChoice::None,
            text: "early".into(),
        },
        DataToBrain::AskAbout {
            turn: 1,
            scene: SceneId(1),
            element: kumochi_scene::Handle::new(1, 1).unwrap(),
        },
    ] {
        let result = queued(&mut brain, async |mut shell| {
            assert_eq!(shell.scenes().await, []);
            let bytes = frame::encode(&message, Pipe::Data).unwrap();
            shell.data_out.write_all(&bytes).await.unwrap();
            while seen.recv().await.unwrap() != Seen::Grace {}
            shell.send(&REGISTER).await;
            closed(&mut shell).await;
        })
        .await;
        assert_eq!(
            result,
            Ended::Broken("turn before queued audio registration")
        );
        while let Ok(event) = seen.try_recv() {
            assert_ne!(event, Seen::ControlApplied);
        }
    }
    brain.close().await;
    assert_eq!(outcomes(&scratch), []);
}

#[tokio::test]
async fn early_hold_controls_fail_before_prepare_capture_or_input_reload() {
    let scratch = Scratch::new("early-queued-hold");
    let mut brain = test_support::brain(&scratch, "");
    let gesture = GestureId(1);
    for message in [
        ControlToBrain::KeyDown { gesture, t_qpc: 1 },
        ControlToBrain::HoldRecognized {
            gesture,
            thread: None,
            selection: 1,
            pointer: PhysicalPoint([0, 0]),
            lease: LeaseGeneration(1),
            t_qpc: 1,
            notice: None,
        },
        ControlToBrain::VoiceHoldRecognized {
            gesture,
            thread: None,
            selection: 1,
        },
        ControlToBrain::ScreenHoldRecognized {
            gesture,
            thread: None,
            selection: 1,
            t_qpc: 1,
            notice: Some(NoticeRevision(std::num::NonZeroU64::MIN)),
        },
    ] {
        let result = queued(&mut brain, async |mut shell| {
            assert_eq!(shell.scenes().await, []);
            shell.send(&message).await;
            shell.send(&REGISTER).await;
            closed(&mut shell).await;
        })
        .await;
        assert_eq!(
            result,
            Ended::Broken("hold before queued audio registration")
        );
    }
    brain.close().await;
    assert_eq!(outcomes(&scratch), []);
}

#[tokio::test]
async fn a_bad_or_withdrawn_output_cannot_emit_a_receipt() {
    let scratch = Scratch::new("stale-queued-output");
    let mut brain = test_support::brain(&scratch, "");
    for message in [
        ControlToBrain::QueuedAudioOutput(MIN_RATE - 1, OUTPUT),
        ControlToBrain::QueuedAudioOutput(MAX_RATE + 1, OUTPUT),
        ControlToBrain::QueuedAudioOutput(24_000, OutputGeneration(0)),
    ] {
        let result = queued(&mut brain, async |mut shell| {
            assert_eq!(shell.scenes().await, []);
            shell.send(&message).await;
            shell.send(&REGISTER).await;
            closed(&mut shell).await;
        })
        .await;
        assert_eq!(result, Ended::Broken("invalid queued audio output"));
    }
    brain.offline(Input::AudioOutput {
        rate: 24_000,
        generation: OUTPUT,
    });
    brain.offline(Input::AudioOutputUnavailable { generation: OUTPUT });
    let result = queued(&mut brain, async |mut shell| {
        assert_eq!(shell.scenes().await, []);
        shell.send(&REGISTER).await;
        closed(&mut shell).await;
    })
    .await;
    assert_eq!(result, Ended::Broken("stale queued audio output"));
    brain.close().await;
}

#[tokio::test]
async fn duplicate_legacy_and_withdrawn_outputs_end_the_pair_after_one_receipt() {
    let scratch = Scratch::new("queued-output-replacement");
    let mut brain = test_support::brain(&scratch, "");
    for (rate, message, reason) in [
        (MIN_RATE, REGISTER, "queued audio registered twice"),
        (
            MAX_RATE,
            ControlToBrain::AudioOutput {
                rate: 24_000,
                generation: OUTPUT,
            },
            "legacy audio on a queued connection",
        ),
        (
            24_000,
            ControlToBrain::AudioOutputUnavailable { generation: OUTPUT },
            "queued audio output withdrawn",
        ),
    ] {
        let result = queued(&mut brain, async |mut shell| {
            assert_eq!(shell.scenes().await, []);
            shell
                .send(&ControlToBrain::QueuedAudioOutput(rate, OUTPUT))
                .await;
            assert_eq!(shell.control().await, Some(RECEIPT));
            shell.send(&message).await;
            shell.send(&REGISTER).await;
            closed(&mut shell).await;
        })
        .await;
        assert_eq!(result, Ended::Broken(reason));
    }
    brain.close().await;
}

#[tokio::test]
async fn a_full_control_queue_refuses_the_receipt_without_opening_admission() {
    let scratch = Scratch::new("queued-receipt-full");
    let mut brain = test_support::brain(&scratch, "");
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let (out, mut control) = mpsc::channel(1);
    let mut tap = Tap::from_env();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    let registration = Cell::new(Registration::Awaiting);
    link.registration = Some(&registration);
    link.control(&ControlToShell::Ping).unwrap();
    assert_eq!(
        brain.control_frame(REGISTER, 0, &mut link, &mut None),
        Err(Ended::Lost)
    );
    assert!(!Registration::admits(link.registration));
    assert_eq!(
        control.try_recv().unwrap(),
        frame::encode(&ControlToShell::Ping, Pipe::Control).unwrap()
    );
    assert_eq!(control.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    brain.close().await;
}
