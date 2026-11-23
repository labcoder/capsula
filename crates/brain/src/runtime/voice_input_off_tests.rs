use super::*;
use crate::runtime::settings;
use crate::voice::LoadReport;
use kumochi_protocol::message::GestureId;
use kumochi_protocol::voice::VoicePart::{SpeechInput, SpokenReplies};

struct Rig {
    brain: Brain,
    models: PathBuf,
    release: std_mpsc::Sender<()>,
    wanted: Box<dyn Fn() -> Option<u64>>,
    fetch: Arc<HeldFetch>,
    fetched: Results,
}

fn attach(scratch: &Scratch, saved: &str, input_short: Option<u64>) -> Rig {
    let models = seeded(scratch, saved, 0);
    if let Some(short) = input_short {
        let input = models.join(SpeechInput.dir());
        std::fs::create_dir_all(&input).unwrap();
        let last = SpeechInput.files().count() - 1;
        for (index, file) in SpeechInput.files().enumerate() {
            let size = if index == last {
                file.bytes - short
            } else {
                file.bytes
            };
            std::fs::File::create(input.join(file.name))
                .unwrap()
                .set_len(size)
                .unwrap();
        }
    }
    let mut brain = test_support::brain(scratch, "");
    let (release, held) = std_mpsc::channel();
    let (stt_reports, stt_heard) = Reports::new();
    let (stt, _) = crate::voice::stt::held_for_runtime(stt_reports, held);
    let wanted = Box::new(crate::voice::stt::wanted_load(&stt));
    let (tts_reports, tts_heard) = Reports::new();
    let tts = TtsHandle::spawn_with(
        tokio::runtime::Handle::current(),
        tts_reports,
        |_: &Path| Ok(Silence),
    )
    .expect("the fake output owner starts");
    let (fetch, fetched) = held_fetch();
    attach_policy_voice(
        &mut brain,
        [input_short == Some(0), true],
        (Some(stt), Some(tts)),
        [stt_heard, tts_heard],
        Arc::<HeldFetch>::clone(&fetch),
    );
    Rig {
        brain,
        models,
        release,
        wanted,
        fetch,
        fetched,
    }
}

async fn owner_report(brain: &mut Brain, part: VoicePart, ask: u64, event: LoadEvent) {
    let next = tokio::time::timeout(
        crate::runtime::tests::PATIENCE,
        runtime_voice::next(&mut brain.voice),
    )
    .await
    .expect("the owner report arrives");
    assert!(
        matches!(next, Event::Report(got_part, LoadReport { ask: got_ask, event: got_event })
        if (got_part, got_ask, got_event) == (part, ask, event))
    );
    brain.voice_event(next, None).unwrap();
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "an exact saved switch value in the reply"
)]
async fn answer(shell: &mut Shell<tokio::io::DuplexStream>, expected_enabled: bool) {
    assert!(matches!(shell.next_control().await.unwrap(),
        Some(ControlToShell::Settings { answer: true, view })
            if view.hold_to_talk == expected_enabled));
}

fn input_files_remain(models: &Path) {
    for file in SpeechInput.files() {
        assert_eq!(
            models
                .join(SpeechInput.dir())
                .join(file.name)
                .metadata()
                .unwrap()
                .len(),
            file.bytes
        );
    }
}

#[tokio::test]
async fn persisted_off_boot_and_manual_load_leave_input_unloaded_but_not_output() {
    let scratch = Scratch::trusted("voice-input-off-startup");
    let Rig {
        mut brain,
        models,
        release,
        wanted,
        fetch,
        ..
    } = attach(
        &scratch,
        "hold_to_talk = false\ntalk_back = true\ninstalled = [\"speech-input\", \"spoken-replies\"]",
        Some(0),
    );
    assert_eq!(wanted(), None);
    owner_report(&mut brain, SpokenReplies, 2, LoadEvent::Loaded).await;
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        let idle = VoiceView {
            speech_input: PartState::Unloaded,
            spoken_replies: PartState::Ready,
        };
        assert_eq!(opening(&mut shell).await, idle);
        shell
            .send(&ControlToBrain::Settings(SettingsOp::LoadSpeechInput))
            .await;
        assert_eq!(
            shell.next_control().await.unwrap(),
            Some(ControlToShell::Error(ErrorKey::HoldToTalkOff.into()))
        );
        answer(&mut shell, false).await;
        assert_eq!(wanted(), None);
        assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
        shell
            .send(&ControlToBrain::Settings(SettingsOp::SetHoldToTalk(true)))
            .await;
        answer(&mut shell, true).await;
        assert_eq!(wanted(), None);
        shell
            .send(&ControlToBrain::KeyDown {
                gesture: GestureId(1),
                t_qpc: 1,
            })
            .await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::Voice(VoiceView {
                speech_input: PartState::Loading,
                ..idle
            }))
        );
        assert_eq!(wanted(), Some(3));
        release.send(()).unwrap();
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::Voice(VoiceView {
                speech_input: PartState::Ready,
                ..idle
            }))
        );
        drop(shell);
    };
    let (ended, ()) = tokio::time::timeout(crate::runtime::tests::PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    assert!(brain.config.view().talk_back);
    assert_eq!(brain.config.installed(), [SpeechInput, SpokenReplies]);
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
    input_files_remain(&models);
    brain.close().await;
}

#[tokio::test]
async fn only_committed_off_releases_input_even_when_the_reply_queue_is_full() {
    let scratch = Scratch::trusted("voice-input-off-full-queue");
    let Rig {
        mut brain,
        models,
        release,
        wanted,
        fetch,
        ..
    } = attach(&scratch, "installed = [\"speech-input\"]", Some(0));
    release.send(()).unwrap();
    owner_report(&mut brain, SpeechInput, 1, LoadEvent::Loaded).await;
    let mut tap = Tap::from_env();
    brain.epoch = 1;
    let off = ControlToBrain::Settings(SettingsOp::SetHoldToTalk(false));
    let blocker = scratch.dir().join(format!("{CONFIG_FILE}.tmp"));
    std::fs::create_dir(&blocker).unwrap();
    let held = brain.hold_config_write();
    brain.control_offline(off.clone(), 0, &mut tap);
    brain.pump_settings_offline();
    assert!(!brain.hold_to_talk_enabled());
    assert_eq!(wanted(), Some(1));
    held.send(Gate::Write).unwrap();
    tokio::time::timeout(crate::runtime::tests::PATIENCE, brain.drain_settings())
        .await
        .unwrap();
    assert!(brain.config.view().hold_to_talk);
    assert_eq!(wanted(), Some(1));
    assert_eq!(brain.hold_voice().0, PartState::Ready);
    std::fs::remove_dir(blocker).unwrap();
    brain.control_offline(off, 0, &mut tap);
    brain.pump_settings_offline();
    let written = tokio::time::timeout(
        crate::runtime::tests::PATIENCE,
        settings::written(&mut brain.config_write),
    )
    .await
    .unwrap();
    assert_eq!(
        voice_on_disk(&scratch)["hold_to_talk"].as_bool(),
        Some(false)
    );
    assert!(brain.config.view().hold_to_talk);
    assert_eq!(wanted(), Some(1));
    let (out, mut unread) = mpsc::channel(1);
    out.try_send(Vec::new()).unwrap();
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    assert!(brain.settings_done(written, Some(&mut link)).is_err());
    assert_eq!(unread.try_recv().unwrap(), Vec::<u8>::new());
    assert_eq!(unread.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert_eq!(wanted(), None);
    assert!(!brain.config.view().hold_to_talk);
    assert_eq!(brain.hold_voice().0, PartState::Loading);
    for (ask, event) in [(1, LoadEvent::Loaded), (2, LoadEvent::Unloaded)] {
        brain
            .voice_event(Event::Report(SpeechInput, LoadReport { ask, event }), None)
            .unwrap();
        assert_eq!(brain.hold_voice().0, PartState::Loading);
    }
    owner_report(&mut brain, SpeechInput, 1, LoadEvent::Unloaded).await;
    let ((control, data), mut shell) = duplex();
    let client = async {
        assert_eq!(opening(&mut shell).await.speech_input, PartState::Unloaded);
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    assert_eq!(brain.config.installed(), [SpeechInput]);
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
    input_files_remain(&models);
    brain.close().await;
}

#[tokio::test]
async fn consumed_off_survives_a_full_hold_status_queue() {
    let scratch = Scratch::trusted("voice-input-off-hold-status");
    let Rig {
        mut brain,
        release,
        wanted,
        ..
    } = attach(&scratch, "installed = [\"speech-input\"]", Some(0));
    release.send(()).unwrap();
    owner_report(&mut brain, SpeechInput, 1, LoadEvent::Loaded).await;
    brain.epoch = 1;
    let mut tap = Tap::from_env();
    let (out, mut unread) = mpsc::channel(1);
    let queue = RefCell::new(DataQueue::new(window()));
    let ready = Notify::new();
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    let mut ping_sent = None;
    assert!(matches!(
        brain.control_frame(
            ControlToBrain::VoiceHoldRecognized {
                gesture: GestureId(1),
                thread: None,
                selection: 1,
            },
            0,
            &mut link,
            &mut ping_sent,
        ),
        Ok(None)
    ));
    out.try_send(Vec::new()).unwrap();
    let held = brain.hold_config_write();
    assert!(matches!(
        brain.control_frame(
            ControlToBrain::Settings(SettingsOp::SetHoldToTalk(false)),
            0,
            &mut link,
            &mut ping_sent,
        ),
        Err(Ended::Lost)
    ));
    assert_eq!(unread.try_recv().unwrap(), Vec::<u8>::new());
    assert_eq!(unread.try_recv(), Err(mpsc::error::TryRecvError::Empty));

    brain.pump_settings_offline();
    assert!(brain.config.view().hold_to_talk);
    assert_eq!(wanted(), Some(1));
    held.send(Gate::Write).unwrap();
    tokio::time::timeout(crate::runtime::tests::PATIENCE, brain.drain_settings())
        .await
        .unwrap();
    assert_eq!(
        voice_on_disk(&scratch)
            .get("hold_to_talk")
            .and_then(toml::Value::as_bool),
        Some(false)
    );
    assert!(!brain.config.view().hold_to_talk);
    assert_eq!(wanted(), None);
    owner_report(&mut brain, SpeechInput, 1, LoadEvent::Unloaded).await;
    assert_eq!(brain.hold_voice().0, PartState::Unloaded);
    brain.close().await;
}

#[tokio::test]
async fn explicit_install_and_retry_while_off_do_the_work_but_do_not_keep_input_loaded() {
    for installed in [false, true] {
        let scratch = Scratch::trusted("voice-input-off-install-or-retry");
        let saved = if installed {
            "hold_to_talk = false\ninstalled = [\"speech-input\"]"
        } else {
            "hold_to_talk = false"
        };
        let Rig {
            mut brain,
            release,
            wanted,
            fetch,
            fetched,
            ..
        } = attach(&scratch, saved, installed.then_some(1));
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            let idle = opening(&mut shell).await;
            assert_eq!(
                idle.speech_input,
                if installed {
                    PartState::Failed {
                        key: ErrorKey::VoiceCorrupt,
                        installed: true,
                    }
                } else {
                    PartState::NotInstalled
                }
            );
            shell
                .send(&ControlToBrain::Settings(SettingsOp::LoadSpeechInput))
                .await;
            assert_eq!(
                shell.next_control().await.unwrap(),
                Some(ControlToShell::Error(ErrorKey::HoldToTalkOff.into()))
            );
            answer(&mut shell, false).await;
            assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
            assert_eq!(wanted(), None);
            shell
                .send(&ControlToBrain::Settings(SettingsOp::InstallVoice(
                    SpeechInput,
                )))
                .await;
            answer(&mut shell, false).await;
            for speech_input in [
                PartState::Verifying,
                PartState::Downloading { done: 3, total: 9 },
            ] {
                assert_eq!(
                    shell.control().await,
                    Some(ControlToShell::Voice(VoiceView {
                        speech_input,
                        ..idle
                    }))
                );
            }
            fetched.send(Ok(())).unwrap();
            assert_eq!(
                shell.control().await,
                Some(ControlToShell::Voice(VoiceView {
                    speech_input: PartState::Loading,
                    ..idle
                }))
            );
            release.send(()).unwrap();
            assert_eq!(
                shell.control().await,
                Some(ControlToShell::Voice(VoiceView {
                    speech_input: PartState::Unloaded,
                    ..idle
                }))
            );
            assert_eq!(wanted(), None);
            drop(shell);
        };
        let (ended, ()) = tokio::time::timeout(crate::runtime::tests::PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .unwrap();
        assert_eq!(ended, Ended::Lost);
        assert_eq!(brain.config.installed(), [SpeechInput]);
        assert_eq!(fetch.begun.load(Ordering::SeqCst), 1);
        brain.close().await;
    }
}
