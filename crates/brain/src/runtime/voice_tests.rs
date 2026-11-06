//! Voice at run time through the core loop: start-up loads, the view on each connection and on
//! each change, and whether runs speak.

use std::path::Path;
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

use super::{Brain, Ended, Shell, duplex};
use crate::coordinator::Input;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};
use crate::voice::Reports;
use crate::voice::tts::{Engine, TtsError, TtsHandle};

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

/// A brain with `voice` as its `[voice]` table and spoken replies' files under its models
/// folder, the last one `short` bytes short. Spoken replies load once the returned sender sends;
/// the counter counts loads begun.
fn attached(
    scratch: &Scratch,
    voice: &str,
    short: u64,
) -> (Brain, std_mpsc::Sender<()>, Arc<AtomicUsize>) {
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
    brain.attach_voice(models, &[], (None, Some(tts)), [stt_heard, tts_heard]);
    brain.offline(Input::AudioOutput {
        rate: 24_000,
        generation: OutputGeneration(1),
    });
    (brain, release, loads)
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
    let (mut brain, release, loads) = attached(
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
    let (mut brain, release, _) = attached(&scratch, "installed = [\"spoken-replies\"]", 0);
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
    brain.close().await;
}

#[tokio::test]
async fn a_part_with_a_short_file_is_corrupt_and_never_loads() {
    let scratch = Scratch::new("voice-short");
    let (mut brain, _release, loads) = attached(
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
