use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use kumochi_protocol::constants::CONFIG_FILE;
use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    AudioSession, ControlToBrain, ControlToShell, DataToBrain, DataToShell, Narration,
    OutputGeneration, PartState, Phase, RunId, ScreenChoice, SettingsOp,
};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Notify, mpsc};

use super::{
    Brain, Ended, Results, Shell, attached, attached_at, duplex, held_fetch, install,
    seed_voice_settings, voice, voice_on_disk,
};
use crate::engines::{Engine, Round, RunRequest, SendGate, TurnEvent, fixture::Fixture};
use crate::runtime::settings::Gate;
use crate::tap::Tap;
use crate::test_support::Scratch;

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

async fn install_spoken_replies(
    shell: &mut Shell<tokio::io::DuplexStream>,
    fetched: &Results,
    load: &Sender<()>,
) -> ControlToShell {
    shell.send(&install()).await;
    assert_eq!(settings(shell).await, (true, Mode::Silent));
    assert_eq!(shell.control().await, Some(voice(PartState::Verifying)));
    assert_eq!(
        shell.control().await,
        Some(voice(PartState::Downloading { done: 3, total: 9 }))
    );
    fetched.send(Ok(())).unwrap();
    assert_eq!(shell.control().await, Some(voice(PartState::Loading)));
    load.send(()).unwrap();
    assert_eq!(shell.control().await, Some(voice(PartState::Ready)));
    current_control(shell).await
}

async fn speak(shell: &mut Shell<tokio::io::DuplexStream>, run: RunId) {
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
}

#[tokio::test]
async fn first_install_keeps_typed_turns_silent_until_a_new_pair_and_respects_saved_off() {
    for (seed, default_on, choose_off_after, saved) in [
        ("", true, false, toml::Value::Boolean(true)),
        ("voice = 3", true, false, toml::Value::Boolean(true)),
        (
            "talk_back = false",
            false,
            false,
            toml::Value::Boolean(false),
        ),
        (
            "talk_back = 'invalid'",
            false,
            false,
            toml::Value::from("invalid"),
        ),
        ("", true, true, toml::Value::Boolean(false)),
    ] {
        let scratch = Scratch::new("voice-queued-first-install");
        let (fetcher, results) = held_fetch();
        if seed.starts_with("voice = ") {
            std::fs::create_dir_all(scratch.dir()).unwrap();
            std::fs::write(scratch.dir().join(CONFIG_FILE), seed).unwrap();
        } else {
            seed_voice_settings(&scratch, seed);
        }
        let (mut brain, load, _) = attached_at(&scratch, None, fetcher);
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
            let notice = install_spoken_replies(&mut shell, &results, &load).await;
            match notice {
                ControlToShell::SpokenRepliesDefaulted(view) if default_on => {
                    assert!(view.talk_back);
                }
                ControlToShell::Settings {
                    answer: false,
                    view,
                } if !default_on => {
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
        assert_eq!(voice_on_disk(&scratch)["talk_back"], saved);
        let ended = queued(&mut brain, async |mut shell| {
            let mode = if default_on && !choose_off_after {
                Mode::Speaking
            } else {
                Mode::Silent
            };
            register(&mut shell, mode, PartState::Ready, OutputGeneration(2)).await;
            let run = submit(&mut shell, 1).await;
            if mode == Mode::Speaking {
                release.notify_one();
                speak(&mut shell, run).await;
            } else {
                text_finishes(&mut shell, run, &release).await;
            }
        })
        .await;
        assert_eq!(ended, Ended::Lost);
        brain.close().await;
    }
}

#[tokio::test]
async fn a_replacement_pair_waits_for_the_previous_off_save_before_it_can_speak() {
    for refused in [false, true] {
        let scratch = Scratch::new("voice-queued-repair-off");
        let (mut brain, load, _, fetched) = attached(&scratch, "");
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
            match install_spoken_replies(&mut shell, &fetched, &load).await {
                ControlToShell::SpokenRepliesDefaulted(view) => assert!(view.talk_back),
                other => panic!("expected deferred default, got {other:?}"),
            }
        })
        .await;
        assert_eq!(ended, Ended::Lost);
        assert_eq!(
            voice_on_disk(&scratch)["talk_back"],
            toml::Value::Boolean(true)
        );
        let held = brain.hold_config_write();
        let off = ControlToBrain::Settings(SettingsOp::SetTalkBack(false));
        let len = postcard::to_allocvec(&off).unwrap().len();
        brain.control_after_loss(off, len, &mut Tap::from_env(), AudioSession::QueuedRequired);
        if refused {
            std::fs::create_dir(scratch.dir().join(format!("{CONFIG_FILE}.tmp"))).unwrap();
            brain.pump_settings_offline();
            super::super::product_waits();
            let ended = queued(&mut brain, async |mut shell| {
                assert_eq!(shell.next_control().await.unwrap(), None);
            })
            .await;
            assert_eq!(ended, Ended::Unavailable);
        }
        let ended = queued(&mut brain, async |mut shell| {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), shell.next_control())
                    .await
                    .is_err(),
                "no Settings or registration is offered while the old Off is held"
            );
            held.send(Gate::Write).unwrap();
            let mode = if refused {
                Mode::Speaking
            } else {
                Mode::Silent
            };
            register(&mut shell, mode, PartState::Ready, OutputGeneration(2)).await;
            let run = submit(&mut shell, 1).await;
            if refused {
                release.notify_one();
                speak(&mut shell, run).await;
            } else {
                text_finishes(&mut shell, run, &release).await;
            }
        })
        .await;
        assert_eq!(ended, Ended::Lost);
        assert_eq!(
            voice_on_disk(&scratch)["talk_back"],
            toml::Value::Boolean(refused)
        );
        brain.close().await;
    }
}
