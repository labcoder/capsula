use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    AudioSession, ControlToBrain, ControlToShell, DataToBrain, DataToShell, Narration,
    OutputGeneration, PartState, Phase, RunId, ScreenChoice, SettingsOp,
};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Notify, mpsc};

use super::{Brain, Ended, Shell, attached, duplex, install, voice, voice_on_disk};
use crate::engines::{Engine, Round, RunRequest, SendGate, TurnEvent, fixture::Fixture};
use crate::tap::Tap;
use crate::test_support::Scratch;
use std::sync::Arc;

#[derive(Debug)]
struct HeldText {
    fixture: Fixture,
    release: Arc<Notify>,
}

impl Engine for HeldText {
    fn start(&self, request: RunRequest, events: mpsc::Sender<TurnEvent>, gate: SendGate) -> Round {
        let round = self.fixture.start(request, events, gate);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            release.notified().await;
            round.await;
        })
    }
}

async fn queued(
    brain: &mut Brain,
    client: impl AsyncFnOnce(Shell<tokio::io::DuplexStream>),
) -> Ended {
    let ((control, data), shell) = duplex();
    let mut tap = Tap::from_env();
    tokio::time::timeout(super::super::PATIENCE, async {
        tokio::join!(
            brain.serve_with_audio(control, data, &mut tap, AudioSession::QueuedRequired),
            client(shell)
        )
        .0
    })
    .await
    .expect("the framed queued session finishes")
}

async fn register(
    shell: &mut Shell<tokio::io::DuplexStream>,
    mode: Mode,
    part: PartState,
    output: OutputGeneration,
) {
    assert_eq!(settings(shell).await, (false, mode));
    assert_eq!(shell.control().await, Some(voice(part)));
    assert_eq!(shell.scenes().await, []);
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Silent,
    Speaking,
}

async fn settings(shell: &mut Shell<tokio::io::DuplexStream>) -> (bool, Mode) {
    match current_control(shell).await {
        ControlToShell::Settings { answer, view } => (
            answer,
            if view.talk_back {
                Mode::Speaking
            } else {
                Mode::Silent
            },
        ),
        other => panic!("expected Settings, got {other:?}"),
    }
}

async fn current_control(shell: &mut Shell<tokio::io::DuplexStream>) -> ControlToShell {
    loop {
        match shell
            .unaccounted()
            .await
            .unwrap()
            .expect("session remains open")
        {
            ControlToShell::Ping => shell.send(&ControlToBrain::Pong).await,
            frame => return frame,
        }
    }
}

async fn submit(shell: &mut Shell<tokio::io::DuplexStream>, turn: u32) -> RunId {
    let message = DataToBrain::TextSubmit {
        turn,
        thread: None,
        screen: ScreenChoice::None,
        text: "go".into(),
    };
    let bytes = frame::encode(&message, Pipe::Data).unwrap();
    shell.data_out.write_all(&bytes).await.unwrap();
    let (run, phase) = shell.state().await;
    assert_eq!(phase, Phase::Thinking);
    run
}

async fn text_finishes(shell: &mut Shell<tokio::io::DuplexStream>, run: RunId, release: &Notify) {
    release.notify_one();
    assert_eq!(shell.state().await, (run, Phase::Answering));
    assert_eq!(
        shell.data().await,
        DataToShell::AssistantText {
            run,
            delta: "Hello there.".into()
        }
    );
    assert_eq!(shell.data().await, DataToShell::RunEnd { run });
    assert_eq!(shell.state().await, (run, Phase::Done));
}

#[tokio::test]
async fn first_install_keeps_typed_turns_silent_until_a_new_pair_and_respects_saved_off() {
    for (seed, choose_off_after, next_mode) in [
        ("", false, true),
        ("talk_back = false", false, false),
        ("", true, false),
    ] {
        let scratch = Scratch::new("voice-queued-first-install");
        let (mut brain, load, _, fetched) = attached(&scratch, seed);
        let release = Arc::new(Notify::new());
        brain.engine = Box::new(HeldText {
            fixture: Fixture::parse("0 text Hello there.").unwrap(),
            release: Arc::clone(&release),
        });
        let ended = queued(&mut brain, async |mut shell| {
            register(
                &mut shell,
                Mode::Silent,
                PartState::NotInstalled,
                OutputGeneration(1),
            )
            .await;
            let active = submit(&mut shell, 1).await;
            shell.send(&install()).await;
            assert_eq!(settings(&mut shell).await, (true, Mode::Silent));
            assert_eq!(shell.control().await, Some(voice(PartState::Verifying)));
            assert_eq!(
                shell.control().await,
                Some(voice(PartState::Downloading { done: 3, total: 9 }))
            );
            fetched.send(Ok(())).unwrap();
            assert_eq!(shell.control().await, Some(voice(PartState::Loading)));
            load.send(()).unwrap();
            assert_eq!(shell.control().await, Some(voice(PartState::Ready)));
            let notice = current_control(&mut shell).await;
            match notice {
                ControlToShell::SpokenRepliesDefaulted(view) if seed.is_empty() => {
                    assert!(view.talk_back);
                }
                ControlToShell::Settings {
                    answer: false,
                    view,
                } if !seed.is_empty() => {
                    assert!(!view.talk_back);
                }
                other => panic!("wrong first-install frame: {other:?}"),
            }
            text_finishes(&mut shell, active, &release).await;
            let another = submit(&mut shell, 2).await;
            text_finishes(&mut shell, another, &release).await;
            if choose_off_after {
                shell
                    .send(&ControlToBrain::Settings(SettingsOp::SetTalkBack(false)))
                    .await;
                assert_eq!(settings(&mut shell).await, (true, Mode::Silent));
            }
        })
        .await;
        assert_eq!(
            ended,
            Ended::Lost,
            "only the actual peer drop ends the session"
        );
        assert_eq!(
            voice_on_disk(&scratch)["talk_back"],
            toml::Value::Boolean(next_mode)
        );
        let ended = queued(&mut brain, async |mut shell| {
            let mode = if next_mode {
                Mode::Speaking
            } else {
                Mode::Silent
            };
            register(&mut shell, mode, PartState::Ready, OutputGeneration(2)).await;
            let run = submit(&mut shell, 1).await;
            if next_mode {
                release.notify_one();
                assert_eq!(shell.state().await, (run, Phase::Answering));
                loop {
                    match shell.data().await {
                        DataToShell::SegmentQueued { run: speaking, .. } => {
                            assert_eq!(speaking, run);
                            break;
                        }
                        DataToShell::RunEnd { .. } => panic!("new registered pair stayed silent"),
                        _ => {}
                    }
                }
            } else {
                text_finishes(&mut shell, run, &release).await;
            }
        })
        .await;
        assert_eq!(ended, Ended::Lost);
        brain.close().await;
    }
}
