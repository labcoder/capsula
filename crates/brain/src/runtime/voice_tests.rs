//! Voice at run time through the core loop: start-up loads, installs, the view on each connection
//! and on each change, and whether runs speak.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;

use kumochi_protocol::constants::CONFIG_FILE;
use kumochi_protocol::message::{
    ControlToBrain, ControlToShell, DataToShell, ErrorKey, OutputGeneration, PartState, Phase,
    SettingsOp, VoiceView,
};
use kumochi_protocol::voice::VoicePart;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch};

use super::{Brain, Ended, Shell, duplex};
use crate::coordinator::Input;
use crate::platform::sherpa::tests::TrustedScratch;
use crate::runtime::settings::Gate;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};
use crate::voice::download::{DownloadError, Fetch, FetchPhase, Progress};
use crate::voice::tts::{Engine, TtsError, TtsHandle};
use crate::voice::{LoadEvent, Reports};

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

/// A fetch that has 3 of 9 bytes downloaded, then ends with the result the test sends, or as
/// cancelled once cancelled. It counts the fetches begun.
#[derive(Debug)]
struct HeldFetch {
    results: tokio::sync::Mutex<mpsc::UnboundedReceiver<Result<(), DownloadError>>>,
    begun: AtomicUsize,
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
            progress.send_replace(Progress {
                phase: FetchPhase::Downloading,
                done: 3,
                total: 9,
            });
            let mut cancel = cancel.clone();
            let mut results = self.results.lock().await;
            tokio::select! {
                Some(result) = results.recv() => result,
                Ok(()) = cancel.changed() => Err(DownloadError::Cancelled),
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
    let models = seeded(scratch, voice, short);
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
    let (fetcher, results) = held_fetch();
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
    (brain, release, loads, results)
}

/// Writes `voice` as the `[voice]` table and spoken replies' files, the last one `short` bytes
/// short; returns the models folder.
fn seeded(scratch: &Scratch, voice: &str, short: u64) -> PathBuf {
    std::fs::create_dir_all(scratch.dir()).expect("scratch folder");
    std::fs::write(
        scratch.dir().join(CONFIG_FILE),
        format!("[voice]\n{voice}\n"),
    )
    .expect("seeded");
    let models = scratch.dir().join("models");
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
    let scratch = Scratch::new("voice-start");
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
    let scratch = Scratch::new("voice-talk-back");
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
    let scratch = Scratch::new("voice-short");
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
    let scratch = Scratch::new("voice-install");
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
    let scratch = Scratch::new("voice-install-choice");
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
    let scratch = Scratch::new("voice-install-cancel");
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
    let scratch = Scratch::new("voice-cleared-refused");
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
    let scratch = Scratch::new("voice-buffered");
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
