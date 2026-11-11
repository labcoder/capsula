//! Voice at run time through the core loop: start-up loads, installs, the view on each connection
//! and on each change, and whether runs speak.

use std::cell::RefCell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;

use kumochi_protocol::constants::CONFIG_FILE;
use kumochi_protocol::frame;
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DataToShell, ErrorKey, OutputGeneration, PartState, Phase,
    SettingsOp, VoiceView,
};
use kumochi_protocol::voice::VoicePart;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, mpsc, watch};

use super::{Brain, Ended, Shell, duplex};
use crate::coordinator::Input;
use crate::platform::sherpa::tests::TrustedScratch;
use crate::runtime::data::DataQueue;
use crate::runtime::settings::Gate;
use crate::runtime::voice::{self as runtime_voice, Event};
use crate::runtime::{Link, Seen, window};
use crate::tap::Tap;
use crate::test_support::{self, Scratch};
use crate::voice::download::{DownloadError, Fetch, FetchPhase, Progress};
use crate::voice::tts::{Engine, TtsError, TtsHandle};
use crate::voice::{LoadEvent, Reports};

/// Returns the owner's idle terminal report without applying it to the installer.
async fn released_input(scratch: &Scratch) -> (Brain, std_mpsc::Sender<()>, Arc<HeldFetch>, Event) {
    use crate::voice::installer::{Durable, Effect, Installer};

    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    std::fs::write(
        scratch.dir().join(CONFIG_FILE),
        "[voice]\ntalk_back = true\ninstalled = [\"speech-input\"]\n",
    )
    .expect("seeded");
    let mut brain = test_support::brain(scratch, "");
    let (release, held) = std_mpsc::channel();
    let (reports, heard) = Reports::new();
    let (stt, _policy) = crate::voice::stt::held_for_runtime(reports, held);
    let (_, tts) = Reports::new();
    let (fetch, _results) = held_fetch();
    brain.attach_voice(
        scratch.dir().join("models"),
        &[],
        (Some(stt), None),
        [heard, tts],
        Arc::<HeldFetch>::clone(&fetch),
    );
    let (installed, effects) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [true; 2].map(Ok),
        [true, false],
        &[],
    );
    brain
        .voice_op(
            |current| {
                *current = installed;
                effects
            },
            None,
        )
        .expect("fixture load");
    release.send(()).expect("release startup load");
    let loaded = tokio::time::timeout(super::PATIENCE, runtime_voice::next(&mut brain.voice))
        .await
        .expect("startup report");
    assert!(matches!(
        loaded,
        Event::Report(
            VoicePart::SpeechInput,
            crate::voice::LoadReport {
                ask: 1,
                event: LoadEvent::Loaded
            }
        )
    ));
    brain.voice_event(loaded, None).expect("startup ready");
    brain
        .voice_op(
            |_| {
                vec![Effect::Unload {
                    part: VoicePart::SpeechInput,
                    ask: 1,
                }]
            },
            None,
        )
        .expect("fixture idle unload");
    let unloaded = tokio::time::timeout(super::PATIENCE, runtime_voice::next(&mut brain.voice))
        .await
        .expect("idle terminal report");
    assert!(matches!(
        unloaded,
        Event::Report(
            VoicePart::SpeechInput,
            crate::voice::LoadReport {
                ask: 1,
                event: LoadEvent::Unloaded
            }
        )
    ));
    (brain, release, fetch, unloaded)
}

fn queued_control(output: &mut mpsc::Receiver<Vec<u8>>) -> ControlToShell {
    let bytes = output.try_recv().expect("one control frame queued");
    frame::decode(&bytes[frame::HEADER_LEN..]).expect("valid control frame")
}

#[tokio::test]
async fn connected_key_down_reloads_input_without_a_turn_or_settings_work() {
    let scratch = Scratch::trusted("voice-key-down");
    let (mut brain, release, fetch, unloaded) = released_input(&scratch).await;
    brain.voice_event(unloaded, None).expect("idle state");

    let (out, mut output) = mpsc::channel(8);
    let queue = RefCell::new(DataQueue::new(window()));
    let (ready, mut tap) = (Notify::new(), Tap::from_env());
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    let mut ping = None;
    let key_down = ControlToBrain::KeyDown {
        gesture: kumochi_protocol::message::GestureId(7),
        t_qpc: 1,
    };
    let loading = VoiceView {
        speech_input: PartState::Loading,
        spoken_replies: PartState::Failed {
            key: ErrorKey::VoiceLoad,
            installed: false,
        },
    };
    assert_eq!(
        brain.control_frame(key_down.clone(), 0, &mut link, &mut ping),
        Ok(None)
    );
    assert_eq!(queued_control(&mut output), ControlToShell::Voice(loading));
    assert_eq!(
        brain.control_frame(key_down.clone(), 0, &mut link, &mut ping),
        Ok(None)
    );
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    release.send(()).expect("release connected reload");
    let loaded = tokio::time::timeout(super::PATIENCE, runtime_voice::next(&mut brain.voice))
        .await
        .expect("reload report");
    assert!(matches!(
        loaded,
        Event::Report(
            VoicePart::SpeechInput,
            crate::voice::LoadReport {
                ask: 3,
                event: LoadEvent::Loaded
            }
        )
    ));
    brain
        .voice_event(loaded, Some(&mut link))
        .expect("reload ready");
    assert_eq!(
        queued_control(&mut output),
        ControlToShell::Voice(VoiceView {
            speech_input: PartState::Ready,
            ..loading
        })
    );
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert!(brain.settings_ops.is_empty());
    assert!(brain.config_write.is_none());
    assert!(brain.config.view().talk_back);
    assert_eq!(brain.coordinator.current_run(), None);
    assert_eq!(
        brain.seen.as_mut().expect("observations").try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );
    assert_eq!(queue.borrow().frames_for_tests(), 0);
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
    brain.close().await;

    let scratch = Scratch::trusted("voice-key-down-unconfigured");
    let mut brain = test_support::brain(&scratch, "");
    assert_eq!(
        brain.control_frame(key_down, 0, &mut link, &mut ping),
        Ok(None)
    );
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    assert!(brain.settings_ops.is_empty());
    assert_eq!(brain.coordinator.current_run(), None);
    brain.close().await;
}

#[tokio::test]
async fn offline_and_early_key_downs_are_dropped_not_replayed_on_reconnect() {
    let scratch = Scratch::trusted("voice-key-down-offline");
    let (mut brain, release, fetch, unloaded) = released_input(&scratch).await;
    let (out, mut output) = mpsc::channel(8);
    let queue = RefCell::new(DataQueue::new(window()));
    let (ready, mut tap) = (Notify::new(), Tap::from_env());
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    let mut ping = None;
    let key_down = ControlToBrain::KeyDown {
        gesture: kumochi_protocol::message::GestureId(8),
        t_qpc: 2,
    };
    assert_eq!(
        brain.control_frame(key_down.clone(), 0, &mut link, &mut ping),
        Ok(None)
    );
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    brain
        .voice_event(unloaded, Some(&mut link))
        .expect("idle report, not a deferred load");
    let idle = VoiceView {
        speech_input: PartState::Unloaded,
        spoken_replies: PartState::Failed {
            key: ErrorKey::VoiceLoad,
            installed: false,
        },
    };
    assert_eq!(queued_control(&mut output), ControlToShell::Voice(idle));
    assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
    drop(link);
    brain.control_offline(key_down.clone(), 0, &mut tap);
    let ((control, data), mut shell) = duplex();
    let client = tokio::time::timeout(super::PATIENCE, async {
        assert_eq!(opening(&mut shell).await, idle);
        shell.send(&key_down).await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::Voice(VoiceView {
                speech_input: PartState::Loading,
                ..idle
            }))
        );
        release.send(()).expect("release fresh connected reload");
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::Voice(VoiceView {
                speech_input: PartState::Ready,
                ..idle
            }))
        );
        drop(shell);
    });
    let (ended, client) = tokio::join!(brain.serve(control, data, &mut tap), client);
    client.expect("reconnect and reload complete in time");
    assert_eq!(ended, Ended::Lost);
    assert_eq!(brain.coordinator.current_run(), None);
    assert!(brain.settings_ops.is_empty());
    assert!(brain.config_write.is_none());
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
    brain.close().await;
}

#[tokio::test]
async fn a_lost_reload_notification_keeps_the_started_load_without_retry() {
    for closed in [false, true] {
        let scratch = Scratch::trusted("voice-key-down-notification");
        let (mut brain, release, fetch, unloaded) = released_input(&scratch).await;
        brain.voice_event(unloaded, None).expect("idle state");
        let (out, mut output) = mpsc::channel(1);
        out.try_send(Vec::new()).expect("fill control output");
        if closed {
            output.close();
        }
        let queue = RefCell::new(DataQueue::new(window()));
        let (ready, mut tap) = (Notify::new(), Tap::from_env());
        let mut link = Link::new(&out, &queue, &ready, &mut tap);
        let mut ping = None;
        let key_down = ControlToBrain::KeyDown {
            gesture: kumochi_protocol::message::GestureId(9),
            t_qpc: 3,
        };
        assert_eq!(
            brain.control_frame(key_down.clone(), 0, &mut link, &mut ping),
            Err(Ended::Lost)
        );
        drop(link);
        assert_eq!(output.try_recv(), Ok(Vec::new()));
        brain.control_offline(key_down, 0, &mut tap);
        release
            .send(())
            .expect("the admitted load survives notification loss");
        let loaded = tokio::time::timeout(super::PATIENCE, runtime_voice::next(&mut brain.voice))
            .await
            .expect("offline load report");
        assert!(matches!(
            loaded,
            Event::Report(
                VoicePart::SpeechInput,
                crate::voice::LoadReport {
                    ask: 3,
                    event: LoadEvent::Loaded
                }
            )
        ));
        brain.voice_event(loaded, None).expect("offline ready");
        let (out, mut output) = mpsc::channel(1);
        let mut link = Link::new(&out, &queue, &ready, &mut tap);
        brain.voice_opened(&mut link).expect("current view");
        assert_eq!(
            queued_control(&mut output),
            ControlToShell::Voice(VoiceView {
                speech_input: PartState::Ready,
                spoken_replies: PartState::Failed {
                    key: ErrorKey::VoiceLoad,
                    installed: false
                }
            })
        );
        assert_eq!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        assert_eq!(brain.coordinator.current_run(), None);
        assert!(brain.settings_ops.is_empty());
        assert!(brain.config_write.is_none());
        assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
        brain.close().await;
    }
}

/// Speaks every segment as one frame of 240 silent samples.
struct Silence;

impl Engine for Silence {
    type Stream = std::iter::Once<Result<Vec<f32>, TtsError>>;
    fn stream(&self, _text: &str) -> Result<Self::Stream, TtsError> {
        Ok(std::iter::once(Ok(vec![0.0; 240])))
    }
}

/// Speech input has no handle, so it's a load failure of a part that isn't installed.
const NO_SPEECH_INPUT: PartState = PartState::Failed {
    key: ErrorKey::VoiceLoad,
    installed: false,
};

#[tokio::test]
async fn idle_policy_reaches_the_owner_only_after_settings_commit_and_installer_readiness() {
    use crate::runtime::{settings, voice as runtime_voice};
    use crate::voice::installer::{Durable, Installer};
    use kumochi_protocol::message::SttUnload;
    use std::time::Duration;
    use tokio::time::timeout;

    let scratch = Scratch::trusted("voice-idle-policy-commit");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    std::fs::write(
        scratch.dir().join(CONFIG_FILE),
        "[voice]\ninstalled = [\"speech-input\"]\n",
    )
    .expect("seeded");
    let mut brain = test_support::brain(&scratch, "");
    let (load, held_load) = std_mpsc::channel();
    let (reports, heard) = Reports::new();
    let (stt, policy) = crate::voice::stt::held_for_runtime(reports, held_load);
    let (_, tts) = Reports::new();
    let (fetch, _results) = held_fetch();
    brain.attach_voice(
        scratch.dir().join("models"),
        &[],
        (Some(stt), None),
        [heard, tts],
        Arc::<HeldFetch>::clone(&fetch),
    );
    let (installed, effects) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [true; 2].map(Ok),
        [true, false],
        &[],
    );
    brain
        .voice_op(
            |current| {
                *current = installed;
                effects
            },
            None,
        )
        .expect("fixture load");
    let mut tap = Tap::from_env();
    let request = |policy| ControlToBrain::Settings(SettingsOp::SetSttUnload(policy));
    let held = brain.hold_config_write();
    brain.control_offline(request(SttUnload::After15Min), 0, &mut tap);
    brain.pump_settings_offline();
    assert!(brain.config_write.is_some());
    assert_eq!(brain.config.view().stt_unload, SttUnload::Kept);
    assert_eq!(policy(), None);
    held.send(Gate::Write).expect("release write");
    timeout(super::PATIENCE, brain.drain_settings())
        .await
        .expect("commit");
    assert_eq!(brain.config.view().stt_unload, SttUnload::After15Min);
    assert_eq!(voice_on_disk(&scratch)["stt_unload"].as_str(), Some("15m"));
    assert_eq!(policy(), None);
    load.send(()).expect("release fake load");
    let loaded = timeout(super::PATIENCE, runtime_voice::next(&mut brain.voice))
        .await
        .expect("loaded");
    brain.voice_event(loaded, None).expect("readiness");
    assert_eq!(policy(), Some(Duration::from_mins(15)));

    let held = brain.hold_config_write();
    brain.control_offline(request(SttUnload::After1Hour), 0, &mut tap);
    brain.pump_settings_offline();
    assert!(brain.config_write.is_some());
    assert_eq!(brain.config.view().stt_unload, SttUnload::After15Min);
    assert_eq!(policy(), Some(Duration::from_mins(15)));
    let blocker = scratch.dir().join(format!("{CONFIG_FILE}.tmp"));
    std::fs::create_dir(&blocker).expect("refuse the durable write");
    held.send(Gate::Write).expect("release refused write");
    timeout(super::PATIENCE, brain.drain_settings())
        .await
        .expect("refused");
    assert_eq!(brain.config.view().stt_unload, SttUnload::After15Min);
    assert_eq!(voice_on_disk(&scratch)["stt_unload"].as_str(), Some("15m"));
    assert_eq!(policy(), Some(Duration::from_mins(15)));
    std::fs::remove_dir(blocker).expect("remove fixture blocker");

    let held = brain.hold_config_write();
    brain.control_offline(request(SttUnload::After1Hour), 0, &mut tap);
    brain.pump_settings_offline();
    assert_eq!(policy(), Some(Duration::from_mins(15)));
    held.send(Gate::Write).expect("release successful write");
    let written = timeout(super::PATIENCE, settings::written(&mut brain.config_write))
        .await
        .expect("durable write completes");
    assert_eq!(voice_on_disk(&scratch)["stt_unload"].as_str(), Some("1h"));
    assert_eq!(brain.config.view().stt_unload, SttUnload::After15Min);
    assert_eq!(policy(), Some(Duration::from_mins(15)));
    brain.settings_done(written, None).expect("commit");
    assert_eq!(brain.config.view().stt_unload, SttUnload::After1Hour);
    assert_eq!(policy(), Some(Duration::from_hours(1)));
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
    brain.close().await;
}

/// A fetch that has 3 of 9 bytes downloaded, then ends with the result the test sends, or as
/// cancelled once cancelled. It counts the fetches begun and cancelled, and notifies each start.
#[derive(Debug)]
struct HeldFetch {
    results: tokio::sync::Mutex<mpsc::UnboundedReceiver<Result<(), DownloadError>>>,
    begun: AtomicUsize,
    cancelled: AtomicUsize,
    started: tokio::sync::Notify,
}

impl Fetch for HeldFetch {
    fn fetch<'a>(
        &'a self,
        _part: VoicePart,
        progress: &'a watch::Sender<Progress>,
        cancel: &'a watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<(), DownloadError>> + Send + 'a>> {
        Box::pin(async move {
            self.begun.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            progress.send_replace(Progress {
                phase: FetchPhase::Downloading,
                done: 3,
                total: 9,
            });
            let mut cancel = cancel.clone();
            let mut results = self.results.lock().await;
            tokio::select! {
                Some(result) = results.recv() => result,
                Ok(()) = cancel.changed() => {
                    self.cancelled.fetch_add(1, Ordering::SeqCst);
                    Err(DownloadError::Cancelled)
                }
                else => Err(DownloadError::Transport),
            }
        })
    }
}

/// Where a test sends how a held fetch ends.
type Results = mpsc::UnboundedSender<Result<(), DownloadError>>;

fn held_fetch() -> (Arc<HeldFetch>, Results) {
    let (results, held) = mpsc::unbounded_channel();
    let fetch = HeldFetch {
        results: tokio::sync::Mutex::new(held),
        begun: AtomicUsize::new(0),
        cancelled: AtomicUsize::new(0),
        started: tokio::sync::Notify::new(),
    };
    (Arc::new(fetch), results)
}

fn voice(spoken_replies: PartState) -> ControlToShell {
    ControlToShell::Voice(VoiceView {
        speech_input: NO_SPEECH_INPUT,
        spoken_replies,
    })
}

/// The `[voice]` table on disk.
fn voice_on_disk(scratch: &Scratch) -> toml::Value {
    let saved: toml::Table = std::fs::read_to_string(scratch.dir().join(CONFIG_FILE))
        .expect("readable")
        .parse()
        .expect("parses");
    saved["voice"].clone()
}

/// A brain with `voice` as its `[voice]` table and spoken replies' files under its models
/// folder, the last one `short` bytes short. Spoken replies load once the returned sender sends;
/// the counter counts loads begun, and a fetch ends with what the last sender sends.
fn attached(
    scratch: &Scratch,
    voice: &str,
    short: u64,
) -> (Brain, std_mpsc::Sender<()>, Arc<AtomicUsize>, Results) {
    let (fetcher, results) = held_fetch();
    let (brain, release, loads) = attached_with(scratch, voice, short, fetcher);
    (brain, release, loads, results)
}

/// As [`attached`], fetching with `fetcher`.
fn attached_with(
    scratch: &Scratch,
    voice: &str,
    short: u64,
    fetcher: Arc<dyn Fetch>,
) -> (Brain, std_mpsc::Sender<()>, Arc<AtomicUsize>) {
    attached_at(scratch, seeded(scratch, voice, short), fetcher)
}

/// As [`attached_with`], with spoken replies' files already under `models`.
fn attached_at(
    scratch: &Scratch,
    models: PathBuf,
    fetcher: Arc<dyn Fetch>,
) -> (Brain, std_mpsc::Sender<()>, Arc<AtomicUsize>) {
    let mut brain = test_support::brain(scratch, "0 text Hello there.");
    let (release, released) = std_mpsc::channel();
    let loads = Arc::new(AtomicUsize::new(0));
    let begun = Arc::clone(&loads);
    let (tts_reports, tts_heard) = Reports::new();
    let tts = TtsHandle::spawn_with(
        tokio::runtime::Handle::current(),
        tts_reports,
        move |_: &Path| {
            begun.fetch_add(1, Ordering::SeqCst);
            released.recv().ok();
            Ok(Silence)
        },
    )
    .expect("the owner thread starts");
    let (_, stt_heard) = Reports::new();
    brain.attach_voice(
        models,
        &[],
        (None, Some(tts)),
        [stt_heard, tts_heard],
        fetcher,
    );
    brain.offline(Input::AudioOutput {
        rate: 24_000,
        generation: OutputGeneration(1),
    });
    (brain, release, loads)
}

/// Writes `voice` as the `[voice]` table and spoken replies' files, the last one `short` bytes
/// short; returns the models folder.
fn seeded(scratch: &Scratch, voice: &str, short: u64) -> PathBuf {
    seeded_at(scratch, scratch.dir().join("models"), voice, short)
}

/// As [`seeded`], with the files under `models`.
fn seeded_at(scratch: &Scratch, models: PathBuf, voice: &str, short: u64) -> PathBuf {
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    std::fs::write(
        scratch.dir().join(CONFIG_FILE),
        format!("[voice]\n{voice}\n"),
    )
    .expect("seeded");
    let dir = models.join(VoicePart::SpokenReplies.dir());
    std::fs::create_dir_all(&dir).expect("the part's folder");
    let files: Vec<_> = VoicePart::SpokenReplies.files().collect();
    for (at, file) in files.iter().enumerate() {
        let size = if at + 1 == files.len() {
            file.bytes - short
        } else {
            file.bytes
        };
        std::fs::File::create(dir.join(file.name))
            .and_then(|made| made.set_len(size))
            .expect("a model file at its size");
    }
    models
}

/// The connection's opening frames up to `Scenes`: the voice view, after checking it comes once,
/// after `Account` and before `Scenes`.
async fn opening<S: AsyncRead + AsyncWrite>(shell: &mut Shell<S>) -> VoiceView {
    let mut kinds = Vec::new();
    let mut voice = None;
    loop {
        match shell.next_control().await.expect("readable").expect("open") {
            ControlToShell::Settings { .. } => kinds.push("Settings"),
            ControlToShell::Account(_) => kinds.push("Account"),
            ControlToShell::Connections(_) => {}
            ControlToShell::Voice(view) => {
                kinds.push("Voice");
                voice = Some(view);
            }
            ControlToShell::Scenes { .. } => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    kinds.push("Scenes");
    assert_eq!(kinds, ["Settings", "Account", "Voice", "Scenes"]);
    voice.expect("a voice view")
}

/// Runs one turn and says whether its answer was spoken, stopping at its first segment.
async fn spoke<S: AsyncRead + AsyncWrite>(shell: &mut Shell<S>) -> bool {
    shell.submit("go").await;
    let (run, _) = shell.state().await;
    assert_eq!(shell.state().await, (run, Phase::Answering));
    loop {
        match shell.data().await {
            DataToShell::SegmentQueued { .. } => return true,
            DataToShell::RunEnd { .. } => return false,
            _ => {}
        }
    }
}

#[tokio::test]
async fn an_installed_part_loads_at_start_and_the_view_follows_it_after_the_account() {
    let scratch = Scratch::trusted("voice-start");
    let (mut brain, release, loads, _) = attached(
        &scratch,
        "talk_back = true\ninstalled = [\"spoken-replies\"]",
        0,
    );
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(
            opening(&mut shell).await,
            VoiceView {
                speech_input: NO_SPEECH_INPUT,
                spoken_replies: PartState::Loading,
            }
        );
        release.send(()).expect("the load waits");
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::Voice(VoiceView {
                speech_input: NO_SPEECH_INPUT,
                spoken_replies: PartState::Ready,
            }))
        );
        assert!(spoke(&mut shell).await, "ready with talk-back on speaks");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ready_spoken_replies_speak_only_once_talk_back_is_on() {
    let scratch = Scratch::trusted("voice-talk-back");
    let (mut brain, release, _, _) = attached(&scratch, "installed = [\"spoken-replies\"]", 0);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opening(&mut shell).await;
        release.send(()).expect("the load waits");
        assert!(matches!(
            shell.control().await,
            Some(ControlToShell::Voice(_))
        ));
        assert!(!spoke(&mut shell).await, "talk-back is off");
        shell
            .send(&ControlToBrain::Settings(SettingsOp::SetTalkBack(true)))
            .await;
        loop {
            if let Some(ControlToShell::Settings { answer: true, view }) =
                shell.next_control().await.expect("readable")
            {
                assert!(view.talk_back);
                break;
            }
        }
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    // The audio output is the connection's own.
    brain.offline(Input::AudioOutput {
        rate: 24_000,
        generation: OutputGeneration(2),
    });
    let ((control, data), mut shell) = duplex();
    let client = async {
        assert_eq!(opening(&mut shell).await.spoken_replies, PartState::Ready);
        assert!(spoke(&mut shell).await, "talk-back turned on speaks");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.offline(Input::AudioOutput {
        rate: 24_000,
        generation: OutputGeneration(3),
    });
    let ((control, data), mut shell) = duplex();
    let client = async {
        opening(&mut shell).await;
        shell
            .send(&ControlToBrain::Settings(SettingsOp::SetTalkBack(false)))
            .await;
        loop {
            if let Some(ControlToShell::Settings { answer: true, view }) =
                shell.next_control().await.expect("readable")
            {
                assert!(!view.talk_back);
                break;
            }
        }
        assert!(
            !spoke(&mut shell).await,
            "talk-back turned off doesn't speak"
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn a_part_with_a_short_file_is_corrupt_and_never_loads() {
    let scratch = Scratch::trusted("voice-short");
    let (mut brain, _release, loads, _) = attached(
        &scratch,
        "talk_back = true\ninstalled = [\"spoken-replies\"]",
        1,
    );
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(
            opening(&mut shell).await,
            VoiceView {
                speech_input: NO_SPEECH_INPUT,
                spoken_replies: PartState::Failed {
                    key: ErrorKey::VoiceCorrupt,
                    installed: true,
                },
            }
        );
        assert!(!spoke(&mut shell).await, "nothing loaded speaks");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(loads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_install_fetches_loads_commits_and_then_speaks() {
    let scratch = Scratch::trusted("voice-install");
    let (mut brain, release, loads, fetched) = attached(&scratch, "", 0);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(
            opening(&mut shell).await.spoken_replies,
            PartState::NotInstalled
        );
        shell
            .send(&ControlToBrain::Settings(SettingsOp::InstallVoice(
                VoicePart::SpokenReplies,
            )))
            .await;
        assert_eq!(shell.control().await, Some(voice(PartState::Verifying)));
        assert_eq!(
            shell.control().await,
            Some(voice(PartState::Downloading { done: 3, total: 9 }))
        );
        fetched.send(Ok(())).expect("the fetch waits");
        assert_eq!(shell.control().await, Some(voice(PartState::Loading)));
        release.send(()).expect("the load waits");
        assert_eq!(shell.control().await, Some(voice(PartState::Ready)));
        assert!(
            spoke(&mut shell).await,
            "the first install turns talk-back on"
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let saved = voice_on_disk(&scratch);
    assert_eq!(
        saved["installed"],
        toml::Value::Array(vec!["spoken-replies".into()])
    );
    assert_eq!(saved["talk_back"], toml::Value::Boolean(true));
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_talk_back_choice_made_during_an_install_wins() {
    let scratch = Scratch::trusted("voice-install-choice");
    let (mut brain, release, _, fetched) = attached(&scratch, "", 0);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opening(&mut shell).await;
        shell
            .send(&ControlToBrain::Settings(SettingsOp::InstallVoice(
                VoicePart::SpokenReplies,
            )))
            .await;
        fetched.send(Ok(())).expect("the fetch waits");
        while shell.control().await != Some(voice(PartState::Loading)) {}
        shell
            .send(&ControlToBrain::Settings(SettingsOp::SetTalkBack(false)))
            .await;
        loop {
            if let Some(ControlToShell::Settings { answer: true, view }) =
                shell.next_control().await.expect("readable")
            {
                assert!(!view.talk_back);
                break;
            }
        }
        release.send(()).expect("the load waits");
        assert_eq!(shell.control().await, Some(voice(PartState::Ready)));
        assert!(!spoke(&mut shell).await, "the user turned talk-back off");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let saved = voice_on_disk(&scratch);
    assert_eq!(saved["talk_back"], toml::Value::Boolean(false));
    assert_eq!(
        saved["installed"],
        toml::Value::Array(vec!["spoken-replies".into()])
    );
}

#[tokio::test]
async fn a_cancelled_install_goes_back_and_the_next_fetches_once_the_first_ends() {
    let scratch = Scratch::trusted("voice-install-cancel");
    let (mut brain, _release, loads, fetched) = attached(&scratch, "", 0);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opening(&mut shell).await;
        let install = ControlToBrain::Settings(SettingsOp::InstallVoice(VoicePart::SpokenReplies));
        shell.send(&install).await;
        assert_eq!(shell.control().await, Some(voice(PartState::Verifying)));
        assert_eq!(
            shell.control().await,
            Some(voice(PartState::Downloading { done: 3, total: 9 }))
        );
        shell
            .send(&ControlToBrain::Settings(SettingsOp::CancelVoiceInstall))
            .await;
        assert_eq!(shell.control().await, Some(voice(PartState::NotInstalled)));
        shell.send(&install).await;
        assert_eq!(shell.control().await, Some(voice(PartState::Verifying)));
        assert_eq!(
            shell.control().await,
            Some(voice(PartState::Downloading { done: 3, total: 9 }))
        );
        fetched
            .send(Err(DownloadError::Refused))
            .expect("the second fetch waits");
        assert_eq!(
            shell.control().await,
            Some(voice(PartState::Failed {
                key: ErrorKey::VoiceDownload,
                installed: false,
            }))
        );
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(loads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_removal_whose_last_write_fails_shows_a_storage_failure_and_fetches_nothing() {
    let scratch = Scratch::trusted("voice-cleared-refused");
    let root = TrustedScratch::new("voice-cleared-refused");
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    std::fs::write(
        scratch.dir().join(CONFIG_FILE),
        "[voice]\nremoving = [\"spoken-replies\"]\n",
    )
    .expect("seeded");
    let mut brain = test_support::brain(&scratch, "");
    let release = brain.hold_config_write();
    let models = root.0.join("models");
    let stuck = brain.finish_removals(&models);
    assert_eq!(stuck, [], "a missing folder counts as deleted");
    let (fetcher, _fetched) = held_fetch();
    let (_, stt_heard) = Reports::new();
    let (_, tts_heard) = Reports::new();
    let shared: Arc<dyn Fetch> = Arc::<HeldFetch>::clone(&fetcher);
    brain.attach_voice(models, &stuck, (None, None), [stt_heard, tts_heard], shared);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(
            opening(&mut shell).await.spoken_replies,
            PartState::NotInstalled
        );
        shell
            .send(&ControlToBrain::Settings(SettingsOp::InstallVoice(
                VoicePart::SpokenReplies,
            )))
            .await;
        release
            .send(Gate::Panic)
            .expect("the write waits on the gate");
        assert_eq!(
            shell.unaccounted().await.expect("readable"),
            Some(voice(PartState::Failed {
                key: ErrorKey::VoiceStorage,
                installed: false,
            }))
        );
        assert!(matches!(
            shell.unaccounted().await.expect("readable"),
            Some(ControlToShell::Settings { answer: true, .. })
        ));
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(
        fetcher.begun.load(Ordering::SeqCst),
        0,
        "nothing fetches around an unfinished removal"
    );
    assert_eq!(
        voice_on_disk(&scratch)["removing"],
        toml::Value::Array(vec!["spoken-replies".into()])
    );
}

#[tokio::test]
async fn a_report_that_came_before_a_connection_is_in_its_opening_view() {
    let scratch = Scratch::trusted("voice-buffered");
    let models = seeded(
        &scratch,
        "talk_back = true\ninstalled = [\"spoken-replies\"]",
        0,
    );
    let mut brain = test_support::brain(&scratch, "0 text Hello there.");
    let (tts_reports, mut owner) = Reports::new();
    let tts = TtsHandle::spawn_with(
        tokio::runtime::Handle::current(),
        tts_reports,
        |_: &Path| Ok(Silence),
    )
    .expect("the owner thread starts");
    let (forward, tts_heard) = mpsc::unbounded_channel();
    let (_, stt_heard) = Reports::new();
    let (fetcher, _fetched) = held_fetch();
    brain.attach_voice(
        models,
        &[],
        (None, Some(tts)),
        [stt_heard, tts_heard],
        fetcher,
    );
    brain.offline(Input::AudioOutput {
        rate: 24_000,
        generation: OutputGeneration(1),
    });
    let loaded = owner.recv().await.expect("the start-up load reports");
    assert_eq!(loaded.event, LoadEvent::Loaded);
    forward.send(loaded).expect("the brain's receiver is open");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(
            opening(&mut shell).await,
            VoiceView {
                speech_input: NO_SPEECH_INPUT,
                spoken_replies: PartState::Ready,
            }
        );
        assert!(spoke(&mut shell).await, "the buffered load is applied");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    drop(forward);
}

fn install() -> ControlToBrain {
    ControlToBrain::Settings(SettingsOp::InstallVoice(VoicePart::SpokenReplies))
}

/// Pumps the queued settings changes to a shell whose control queue is already full.
fn pump_full(brain: &mut Brain) {
    let (out, _unread) = mpsc::channel(1);
    out.try_send(Vec::new()).expect("room for one");
    let queue = RefCell::new(DataQueue::new(window()));
    let (ready, mut tap) = (Notify::new(), Tap::from_env());
    let mut link = Link::new(&out, &queue, &ready, &mut tap);
    assert!(brain.pump_settings(Some(&mut link)).is_err(), "full");
}

#[tokio::test]
async fn an_install_whose_answer_finds_the_queue_full_still_fetches() {
    let scratch = Scratch::trusted("voice-install-full");
    let (fetch, _results) = held_fetch();
    let (mut brain, _release, _) = attached_with(&scratch, "", 0, Arc::<HeldFetch>::clone(&fetch));
    let mut tap = Tap::from_env();
    brain.control_offline(install(), 0, &mut tap);
    pump_full(&mut brain);
    tokio::time::timeout(super::PATIENCE, fetch.started.notified())
        .await
        .expect("the fetch starts");
    brain.close().await;
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_cancel_whose_answer_finds_the_queue_full_still_cancels() {
    let scratch = Scratch::trusted("voice-cancel-full");
    let (fetch, _results) = held_fetch();
    let (mut brain, _release, _) = attached_with(&scratch, "", 0, Arc::<HeldFetch>::clone(&fetch));
    let mut tap = Tap::from_env();
    brain.control_offline(install(), 0, &mut tap);
    brain.pump_settings_offline();
    tokio::time::timeout(super::PATIENCE, fetch.started.notified())
        .await
        .expect("the fetch starts");
    brain.control_offline(
        ControlToBrain::Settings(SettingsOp::CancelVoiceInstall),
        0,
        &mut tap,
    );
    pump_full(&mut brain);
    tokio::time::timeout(super::PATIENCE, async {
        while fetch.cancelled.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the fetch is cancelled before quit");
    brain.close().await;
}

#[tokio::test]
async fn quit_stops_the_running_fetch() {
    let scratch = Scratch::trusted("voice-quit-fetch");
    let (fetch, _results) = held_fetch();
    let (mut brain, _release, _) = attached_with(&scratch, "", 0, Arc::<HeldFetch>::clone(&fetch));
    let mut tap = Tap::from_env();
    brain.control_offline(install(), 0, &mut tap);
    brain.pump_settings_offline();
    tokio::time::timeout(super::PATIENCE, fetch.started.notified())
        .await
        .expect("the fetch starts");
    tokio::time::timeout(
        super::PATIENCE,
        brain.quit(tokio::time::Instant::now() + super::PATIENCE),
    )
    .await
    .expect("quit ends");
    assert_eq!(fetch.cancelled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn quit_cancels_the_fetch_and_keeps_the_held_write() {
    let scratch = Scratch::trusted("voice-quit-drain");
    let (fetch, _results) = held_fetch();
    let (mut brain, _release, _) = attached_with(&scratch, "", 0, Arc::<HeldFetch>::clone(&fetch));
    let mut tap = Tap::from_env();
    brain.control_offline(install(), 0, &mut tap);
    brain.pump_settings_offline();
    tokio::time::timeout(super::PATIENCE, fetch.started.notified())
        .await
        .expect("the fetch starts");
    let release = brain.hold_config_write();
    brain.control_offline(
        ControlToBrain::Settings(SettingsOp::SetTalkBack(true)),
        0,
        &mut tap,
    );
    brain.pump_settings_offline();
    release
        .send(Gate::Write)
        .expect("the write waits on the gate");
    tokio::time::timeout(
        super::PATIENCE,
        brain.quit(tokio::time::Instant::now() + super::PATIENCE),
    )
    .await
    .expect("quit ends");
    assert_eq!(fetch.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(
        voice_on_disk(&scratch)["talk_back"],
        toml::Value::Boolean(true)
    );
}

#[tokio::test]
async fn an_install_queued_behind_a_write_never_fetches_after_quit() {
    let scratch = Scratch::trusted("voice-quit-queued");
    let (fetch, _results) = held_fetch();
    let (mut brain, _release, _) = attached_with(&scratch, "", 0, Arc::<HeldFetch>::clone(&fetch));
    let mut tap = Tap::from_env();
    let release = brain.hold_config_write();
    brain.control_offline(
        ControlToBrain::Settings(SettingsOp::SetTalkBack(true)),
        0,
        &mut tap,
    );
    brain.pump_settings_offline();
    brain.control_offline(install(), 0, &mut tap);
    release
        .send(Gate::Write)
        .expect("the write waits on the gate");
    tokio::time::timeout(
        super::PATIENCE,
        brain.quit(tokio::time::Instant::now() + super::PATIENCE),
    )
    .await
    .expect("quit ends");
    tokio::task::yield_now().await;
    assert_eq!(
        voice_on_disk(&scratch)["talk_back"],
        toml::Value::Boolean(true)
    );
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_install_finishes_while_no_shell_is_connected() {
    let scratch = Scratch::trusted("voice-install-idle");
    let (fetch, results) = held_fetch();
    let (mut brain, release, loads) =
        attached_with(&scratch, "", 0, Arc::<HeldFetch>::clone(&fetch));
    let mut seen = brain.observe();
    let mut tap = Tap::from_env();
    brain.control_offline(install(), 0, &mut tap);
    results.send(Ok(())).expect("the fetch waits");
    release.send(()).expect("the load waits");
    let committed =
        async { while seen.recv().await.expect("the brain reports") != Seen::SettingsCommitted {} };
    tokio::select! {
        () = brain.idle() => unreachable!("idle never returns"),
        committed = tokio::time::timeout(super::PATIENCE, committed) => {
            committed.expect("the install commits with no shell connected");
        }
    }
    let saved = voice_on_disk(&scratch);
    assert_eq!(
        saved["installed"],
        toml::Value::Array(vec!["spoken-replies".into()])
    );
    assert_eq!(saved["talk_back"], toml::Value::Boolean(true));
    let ((control, data), mut shell) = duplex();
    let client = async {
        assert_eq!(opening(&mut shell).await.spoken_replies, PartState::Ready);
        assert!(spoke(&mut shell).await, "the installed part speaks");
        drop(shell);
    };
    let (ended, ()) = tokio::time::timeout(super::PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("the shell sees the installed part");
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 1);
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

/// Spoken replies installed under a trusted models folder, with talk-back on; it loads once the
/// returned sender sends. Returns its folder too.
fn removable(scratch: &Scratch, root: &TrustedScratch) -> (Brain, std_mpsc::Sender<()>, PathBuf) {
    let models = seeded_at(
        scratch,
        root.0.join("models"),
        "talk_back = true\ninstalled = [\"spoken-replies\"]",
        0,
    );
    let (fetcher, _) = held_fetch();
    let folder = models.join(VoicePart::SpokenReplies.dir());
    let (brain, release, _) = attached_at(scratch, models, fetcher);
    (brain, release, folder)
}

fn remove() -> ControlToBrain {
    ControlToBrain::Settings(SettingsOp::RemoveVoice(VoicePart::SpokenReplies))
}

/// Row 5: until the intent commits, the part keeps its model and speaks; a refused intent is the
/// op's answer and changes nothing.
#[tokio::test]
async fn a_refused_removal_keeps_the_part_speaking_and_its_files() {
    let scratch = Scratch::trusted("voice-remove-refused");
    let root = TrustedScratch::new("voice-remove-refused");
    let (mut brain, release, folder) = removable(&scratch, &root);
    let gate = brain.hold_config_write();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opening(&mut shell).await;
        release.send(()).expect("the load waits");
        assert_eq!(shell.control().await, Some(voice(PartState::Ready)));
        shell.send(&remove()).await;
        assert!(spoke(&mut shell).await, "served while the intent is held");
        gate.send(Gate::Panic).expect("the write waits on the gate");
        assert!(matches!(
            shell.unaccounted().await.expect("readable"),
            Some(ControlToShell::Error(error)) if error.key() == ErrorKey::SettingsRefused
        ));
        assert!(matches!(
            shell.unaccounted().await.expect("readable"),
            Some(ControlToShell::Settings { answer: true, .. })
        ));
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert!(folder.is_dir());
    let saved = voice_on_disk(&scratch);
    assert_eq!(
        saved["installed"],
        toml::Value::Array(vec!["spoken-replies".into()])
    );
    assert_eq!(saved.get("removing"), None);
}

/// Row 6: a committed removal is the op's answer and stops the part speaking, then its folder is
/// deleted and the removal written as finished.
#[tokio::test]
async fn a_removal_answers_with_its_intent_then_deletes_and_finishes() {
    let scratch = Scratch::trusted("voice-remove");
    let root = TrustedScratch::new("voice-remove");
    let (mut brain, release, folder) = removable(&scratch, &root);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opening(&mut shell).await;
        release.send(()).expect("the load waits");
        assert_eq!(shell.control().await, Some(voice(PartState::Ready)));
        shell.send(&remove()).await;
        assert_eq!(
            shell.unaccounted().await.expect("readable"),
            Some(voice(PartState::NotInstalled))
        );
        assert!(matches!(
            shell.unaccounted().await.expect("readable"),
            Some(ControlToShell::Settings { answer: true, .. })
        ));
        assert!(
            matches!(
                shell.unaccounted().await.expect("readable"),
                Some(ControlToShell::Settings { answer: false, .. })
            ),
            "the removal's last write"
        );
        assert!(!spoke(&mut shell).await, "removed parts don't speak");
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    assert!(!folder.exists(), "the part's folder is deleted");
    assert!(
        root.0.join("models").is_dir(),
        "only the part's folder goes"
    );
    let saved = voice_on_disk(&scratch);
    assert_eq!(saved["installed"], toml::Value::Array(Vec::new()));
    assert_eq!(saved["removing"], toml::Value::Array(Vec::new()));
}

/// Row 19: Quit lets a queued intent land and stops the removal there, so start-up finishes it.
#[tokio::test]
async fn quit_lands_a_held_intent_and_leaves_the_delete_to_start_up() {
    let scratch = Scratch::trusted("voice-remove-quit");
    let root = TrustedScratch::new("voice-remove-quit");
    let (mut brain, _release, folder) = removable(&scratch, &root);
    let gate = brain.hold_config_write();
    let mut tap = Tap::from_env();
    brain.control_offline(remove(), 0, &mut tap);
    brain.pump_settings_offline();
    gate.send(Gate::Write).expect("the write waits on the gate");
    tokio::time::timeout(
        super::PATIENCE,
        brain.quit(tokio::time::Instant::now() + super::PATIENCE),
    )
    .await
    .expect("quit ends");
    let saved = voice_on_disk(&scratch);
    assert_eq!(saved["installed"], toml::Value::Array(Vec::new()));
    assert_eq!(
        saved["removing"],
        toml::Value::Array(vec!["spoken-replies".into()])
    );
    assert!(
        folder.is_dir(),
        "the load never ended, so nothing was deleted"
    );
}

/// Row 19: an intent held past Quit's deadline still lands once released, and the file says so.
#[tokio::test]
async fn an_intent_held_past_quit_still_lands_on_disk() {
    let scratch = Scratch::trusted("voice-remove-late");
    let root = TrustedScratch::new("voice-remove-late");
    let (mut brain, _release, _) = removable(&scratch, &root);
    let gate = brain.hold_config_write();
    let mut tap = Tap::from_env();
    brain.control_offline(remove(), 0, &mut tap);
    brain.pump_settings_offline();
    tokio::time::timeout(
        super::PATIENCE,
        brain.quit(tokio::time::Instant::now() + std::time::Duration::from_millis(50)),
    )
    .await
    .expect("quit ends at its deadline");
    assert_eq!(
        voice_on_disk(&scratch).get("removing"),
        None,
        "not written yet"
    );
    let (completed, written) = tokio::sync::oneshot::channel();
    gate.send(Gate::WriteAndSignal(completed))
        .expect("the write waits on the gate");
    assert!(
        tokio::time::timeout(super::PATIENCE, written)
            .await
            .expect("the held write lands")
            .expect("the writer signals its result")
    );
    let saved = voice_on_disk(&scratch);
    assert_eq!(saved["installed"], toml::Value::Array(Vec::new()));
    assert_eq!(
        saved["removing"],
        toml::Value::Array(vec!["spoken-replies".into()])
    );
}

/// A fresh spoken-replies install whose Activate waits in the settings queue behind a removal:
/// a talk-back write is held, the removal queues behind it, and the load then finishes. `intent`
/// is what the removal's intent write does once it's staged, `None` to write it as usual. Returns the brain, the held write's gate and the
/// part's folder.
async fn activate_behind_a_removal(
    scratch: &Scratch,
    root: &TrustedScratch,
    intent: Option<Gate>,
) -> (Brain, std_mpsc::Sender<Gate>, PathBuf) {
    let models = seeded_at(
        scratch,
        root.0.join("models"),
        "talk_back = true\ninstalled = [\"speech-input\"]",
        0,
    );
    let folder = models.join(VoicePart::SpokenReplies.dir());
    let (fetch, results) = held_fetch();
    let (mut brain, release, _) = attached_at(scratch, models, Arc::<HeldFetch>::clone(&fetch));
    let mut tap = Tap::from_env();
    brain.control_offline(install(), 0, &mut tap);
    brain.pump_settings_offline();
    tokio::time::timeout(super::PATIENCE, fetch.started.notified())
        .await
        .expect("the fetch starts");
    let held = brain.hold_config_write();
    brain.control_offline(
        ControlToBrain::Settings(SettingsOp::SetTalkBack(false)),
        0,
        &mut tap,
    );
    brain.pump_settings_offline();
    brain.control_offline(remove(), 0, &mut tap);
    results.send(Ok(())).expect("the fetch waits");
    release.send(()).expect("the load waits");
    tokio::time::timeout(super::PATIENCE, async {
        while !brain.voice_activating(VoicePart::SpokenReplies, 3) {
            tokio::time::timeout(TICK, brain.idle())
                .await
                .expect_err("idle never returns");
        }
    })
    .await
    .expect("the load finishes and its Activate queues");
    if let Some(gate) = intent {
        let held = brain.hold_config_write();
        held.send(gate).expect("the intent waits on the gate");
    }
    (brain, held, folder)
}

/// Runs the brain with no shell until `done` holds for what's on disk.
async fn idle_until(brain: &mut Brain, scratch: &Scratch, done: impl Fn(&toml::Value) -> bool) {
    let mut saved = None;
    let waited = tokio::time::timeout(super::PATIENCE, async {
        loop {
            if brain.config_write.is_none() && brain.settings_ops.is_empty() {
                let disk = voice_on_disk(scratch);
                let finished = done(&disk);
                saved = Some(disk);
                if finished {
                    break;
                }
            }
            tokio::time::timeout(TICK, brain.idle())
                .await
                .expect_err("idle never returns");
        }
    })
    .await;
    if waited.is_err() {
        eprintln!("{}", timeout_diagnostics(brain, saved.as_ref()));
    }
    waited.expect("the writes land");
}

#[derive(Debug, PartialEq, Eq)]
struct DiskParts {
    kind: &'static str,
    count: Option<usize>,
    known: Option<[bool; 2]>,
}

fn disk_parts(saved: Option<&toml::Value>, key: &str) -> Option<DiskParts> {
    saved.and_then(|disk| disk.get(key)).map(|field| DiskParts {
        kind: field.type_str(),
        count: field.as_array().map(Vec::len),
        known: field.as_array().map(|values| {
            ["speech-input", "spoken-replies"]
                .map(|name| values.iter().any(|value| value.as_str() == Some(name)))
        }),
    })
}

fn timeout_diagnostics(brain: &Brain, saved: Option<&toml::Value>) -> String {
    format!(
        "voice write timeout: voice={:?} write(kind,finished)={:?} queued_writes={} memory_installed={:?} memory_removing={:?} disk_installed={:?} disk_removing={:?} disk_talk_back={:?}",
        brain
            .voice
            .as_ref()
            .map(crate::runtime::voice::Voice::diagnostics),
        brain
            .config_write
            .as_ref()
            .map(crate::runtime::settings::Write::diagnostics),
        brain.settings_ops.len(),
        brain.config.installed(),
        brain.config.removing(),
        disk_parts(saved, "installed"),
        disk_parts(saved, "removing"),
        saved
            .and_then(|disk| disk.get("talk_back"))
            .and_then(toml::Value::as_bool),
    )
}

#[tokio::test]
async fn model_review_debug_excludes_paths_through_outer_carriers() {
    let scratch = Scratch::trusted("voice-debug-path");
    let models = scratch.dir().join(test_support::SENTINEL);
    let fetcher: Arc<dyn Fetch> = Arc::new(crate::voice::download::Online(models.clone()));
    let (brain, _release, _) = attached_at(&scratch, models, Arc::clone(&fetcher));
    let debug = format!("{brain:?} {fetcher:?}");
    assert!(!debug.contains(test_support::SENTINEL));
    assert!(!debug.contains(&format!("{:?}", test_support::SENTINEL.as_bytes())));
    brain.close().await;
}

#[tokio::test]
async fn removal_diagnostics_exclude_paths_and_report_owner_counts() {
    let scratch = Scratch::trusted("voice-diagnostics");
    let (fetch, _results) = held_fetch();
    let (brain, _release, _) =
        attached_at(&scratch, scratch.dir().join(test_support::SENTINEL), fetch);
    let mut disk = "installed = [\"speech-input\"]"
        .parse::<toml::Table>()
        .expect("fixture");
    disk.insert("private".into(), test_support::SENTINEL.into());
    let disk = toml::Value::Table(disk);
    let diagnostic = timeout_diagnostics(&brain, Some(&disk));
    assert!(!diagnostic.contains(test_support::SENTINEL));
    assert!(!diagnostic.contains(&format!("{:?}", test_support::SENTINEL.as_bytes())));
    assert_eq!(diagnostic.matches("owners: 0").count(), 2);
    assert_eq!(
        disk_parts(Some(&disk), "installed"),
        Some(DiskParts {
            kind: "array",
            count: Some(1),
            known: Some([true, false])
        })
    );
    brain.close().await;
}

/// How long a test lets the brain run before it looks again.
const TICK: std::time::Duration = std::time::Duration::from_millis(10);

/// An Activate queued behind a removal is never written once the removal commits, so the part
/// doesn't come back in `installed` when the removal finishes.
#[tokio::test]
async fn an_activate_queued_behind_a_committed_removal_isnt_written() {
    let scratch = Scratch::trusted("voice-remove-activate");
    let root = TrustedScratch::new("voice-remove-activate");
    let (mut brain, held, folder) = activate_behind_a_removal(&scratch, &root, None).await;
    held.send(Gate::Write).expect("the write waits on the gate");
    idle_until(&mut brain, &scratch, |saved| {
        saved.get("removing") == Some(&toml::Value::Array(Vec::new()))
    })
    .await;
    brain.close().await;
    assert!(!folder.exists(), "the part's folder is deleted");
    let saved = voice_on_disk(&scratch);
    assert_eq!(
        saved["installed"],
        toml::Value::Array(vec!["speech-input".into()])
    );
    assert_eq!(saved["removing"], toml::Value::Array(Vec::new()));
    assert_eq!(saved["talk_back"], toml::Value::Boolean(false));
    let restarted = crate::config::Config::load(
        Some(scratch.dir().to_owned()),
        kumochi_protocol::message::ThreadId(1),
    );
    assert_eq!(restarted.installed(), [VoicePart::SpeechInput]);
    assert_eq!(restarted.removing(), []);
}

/// Behind a refused removal, the same Activate is written and the part is ready.
#[tokio::test]
async fn an_activate_queued_behind_a_refused_removal_still_lands() {
    let scratch = Scratch::trusted("voice-remove-activate-refused");
    let root = TrustedScratch::new("voice-remove-activate-refused");
    let (mut brain, held, folder) =
        activate_behind_a_removal(&scratch, &root, Some(Gate::Panic)).await;
    held.send(Gate::Write).expect("the write waits on the gate");
    idle_until(&mut brain, &scratch, |saved| {
        saved.get("installed")
            == Some(&toml::Value::Array(vec![
                "speech-input".into(),
                "spoken-replies".into(),
            ]))
    })
    .await;
    let saved = voice_on_disk(&scratch);
    assert_eq!(
        saved["installed"],
        toml::Value::Array(vec!["speech-input".into(), "spoken-replies".into()])
    );
    assert_eq!(saved.get("removing"), None);
    assert_eq!(saved["talk_back"], toml::Value::Boolean(false));
    assert!(folder.is_dir());
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(opening(&mut shell).await.spoken_replies, PartState::Ready);
        drop(shell);
    };
    let (ended, ()) = tokio::time::timeout(super::PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .expect("the shell sees the part");
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn a_cancelled_native_load_keeps_its_files_until_its_owner_finishes() {
    let scratch = Scratch::trusted("voice-cancel-load-remove");
    let root = TrustedScratch::new("voice-cancel-load-remove");
    let models = seeded_at(&scratch, root.0.join("models"), "", 0);
    let folder = models.join(VoicePart::SpokenReplies.dir());
    let (fetch, results) = held_fetch();
    let (mut brain, release, loads) = attached_at(&scratch, models, fetch);
    let mut tap = Tap::from_env();
    brain.control_offline(install(), 0, &mut tap);
    brain.pump_settings_offline();
    results.send(Ok(())).expect("the fetch waits");
    tokio::time::timeout(super::PATIENCE, async {
        while loads.load(Ordering::SeqCst) != 1 {
            tokio::time::timeout(TICK, brain.idle())
                .await
                .expect_err("idle never returns");
        }
    })
    .await
    .expect("the native load starts");
    brain.control_offline(
        ControlToBrain::Settings(SettingsOp::CancelVoiceInstall),
        0,
        &mut tap,
    );
    brain.pump_settings_offline();
    brain.control_offline(remove(), 0, &mut tap);
    brain.pump_settings_offline();
    idle_until(&mut brain, &scratch, |saved| {
        saved.get("removing") == Some(&toml::Value::Array(vec!["spoken-replies".into()]))
    })
    .await;
    assert!(folder.is_dir(), "the cancelled owner still holds its model");
    assert!(!brain.voice_deleting(VoicePart::SpokenReplies));
    release.send(()).expect("the load is still waiting");
    idle_until(&mut brain, &scratch, |saved| {
        saved.get("removing") == Some(&toml::Value::Array(Vec::new()))
    })
    .await;
    brain.close().await;
    assert!(
        !folder.exists(),
        "the terminal owner report permits deletion"
    );
    assert_eq!(loads.load(Ordering::SeqCst), 1);
}

#[derive(Debug)]
struct RetiringFetch {
    cancelled: Notify,
    finish: Notify,
}

impl Fetch for RetiringFetch {
    fn fetch<'a>(
        &'a self,
        _part: VoicePart,
        _progress: &'a watch::Sender<Progress>,
        cancel: &'a watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<(), DownloadError>> + Send + 'a>> {
        Box::pin(async move {
            let mut cancel = cancel.clone();
            while !*cancel.borrow_and_update() {
                cancel
                    .changed()
                    .await
                    .expect("the runtime owns cancellation");
            }
            self.cancelled.notify_one();
            self.finish.notified().await;
            Err(DownloadError::Cancelled)
        })
    }
}

#[tokio::test]
async fn a_cancelled_fetch_keeps_its_files_until_its_task_finishes() {
    let scratch = Scratch::trusted("voice-cancel-fetch-remove");
    let root = TrustedScratch::new("voice-cancel-fetch-remove");
    let models = seeded_at(&scratch, root.0.join("models"), "", 0);
    let folder = models.join(VoicePart::SpokenReplies.dir());
    let fetch = Arc::new(RetiringFetch {
        cancelled: Notify::new(),
        finish: Notify::new(),
    });
    let (mut brain, _release, _) =
        attached_at(&scratch, models, Arc::<RetiringFetch>::clone(&fetch));
    let mut tap = Tap::from_env();
    brain.control_offline(install(), 0, &mut tap);
    brain.pump_settings_offline();
    brain.control_offline(
        ControlToBrain::Settings(SettingsOp::CancelVoiceInstall),
        0,
        &mut tap,
    );
    brain.pump_settings_offline();
    tokio::time::timeout(super::PATIENCE, fetch.cancelled.notified())
        .await
        .expect("the fetch sees cancellation but hasn't ended");
    brain.control_offline(remove(), 0, &mut tap);
    brain.pump_settings_offline();
    idle_until(&mut brain, &scratch, |saved| {
        saved.get("removing") == Some(&toml::Value::Array(vec!["spoken-replies".into()]))
    })
    .await;
    assert!(folder.is_dir(), "the fetch hasn't returned yet");
    assert!(!brain.voice_deleting(VoicePart::SpokenReplies));
    fetch.finish.notify_one();
    idle_until(&mut brain, &scratch, |saved| {
        saved.get("removing") == Some(&toml::Value::Array(Vec::new()))
    })
    .await;
    brain.close().await;
    assert!(
        !folder.exists(),
        "the fetch's terminal result permits deletion"
    );
}
