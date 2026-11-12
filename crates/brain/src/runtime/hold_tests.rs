use std::sync::Arc;
use std::sync::atomic::Ordering;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    ConnectionKind, ControlToBrain, ControlToShell, DataToBrain, DataToShell, Effort, GestureId,
    HoldFailure, HoldStatus, MicPcm, ModelChoice, ModelSlug, RunOrigin, ThreadId,
};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;

use super::{Brain, Ended, Shell, duplex};
use crate::runtime::voice;
use crate::store::Store;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};
use crate::voice::Reports;
use crate::voice::installer::{Durable, Installer};
use crate::voice::stt::held_for_runtime_checked;

const G: GestureId = GestureId(17);

struct Requested(mpsc::UnboundedSender<crate::engines::RunRequest>);

impl std::fmt::Debug for Requested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Requested")
    }
}

impl crate::engines::Engine for Requested {
    fn start(
        &self,
        request: crate::engines::RunRequest,
        events: mpsc::Sender<crate::engines::TurnEvent>,
        gate: crate::engines::SendGate,
    ) -> crate::engines::Round {
        let seen = self.0.clone();
        Box::pin(async move {
            let crate::engines::Answer::Grant(lease) = gate.ask().await else {
                return;
            };
            seen.send(request).unwrap();
            for event in [
                crate::engines::TurnEvent::TextDelta("selected answer".into()),
                crate::engines::TurnEvent::Finished(crate::engines::Finish::Completed),
            ] {
                if events.send(event).await.is_err() {
                    return;
                }
            }
            drop(lease);
        })
    }
}

pub(in crate::runtime) type InputVoice = (
    Brain,
    std::sync::mpsc::Sender<()>,
    mpsc::UnboundedReceiver<()>,
    Arc<super::voice::HeldFetch>,
);

pub(in crate::runtime) async fn input(
    scratch: &Scratch,
    expected: Vec<i16>,
    text: &'static str,
) -> InputVoice {
    let (mut brain, release, heard_decode, fetch) = loading_input(scratch, expected, text);
    release.send(()).unwrap();
    let event = tokio::time::timeout(super::PATIENCE, voice::next(&mut brain.voice))
        .await
        .unwrap();
    brain.voice_event(event, None).unwrap();
    (brain, release, heard_decode, fetch)
}

pub(in crate::runtime) fn loading_input(
    scratch: &Scratch,
    expected: Vec<i16>,
    text: &'static str,
) -> InputVoice {
    let mut brain = test_support::brain(scratch, "0 text selected answer");
    let (release, held) = std::sync::mpsc::channel();
    let (began, heard_decode) = mpsc::unbounded_channel();
    let (reports, heard) = Reports::new();
    let (stt, _policy) = held_for_runtime_checked(reports, held, move |rate, samples| {
        assert_eq!(rate, 16_000);
        assert_eq!(samples, expected);
        began.send(()).expect("decode observed");
        text.into()
    });
    let (_, tts) = Reports::new();
    let (fetch, _results) = super::voice::held_fetch();
    brain.attach_voice(
        scratch.dir().join("models"),
        &[],
        (Some(stt), None),
        [heard, tts],
        Arc::<super::voice::HeldFetch>::clone(&fetch),
    );
    let (installed, effects) = Installer::start(
        [Durable::Installed, Durable::Absent],
        [Ok(true); 2],
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
        .unwrap();
    (brain, release, heard_decode, fetch)
}

async fn status(shell: &mut Shell<tokio::io::DuplexStream>, expected: HoldStatus) {
    status_for(shell, G, expected).await;
}

async fn status_for(
    shell: &mut Shell<tokio::io::DuplexStream>,
    owner: GestureId,
    expected: HoldStatus,
) {
    loop {
        match shell.control().await {
            Some(ControlToShell::Voice(_)) => {}
            Some(ControlToShell::VoiceHoldStatus { gesture, status }) => {
                assert_eq!((gesture, status), (owner, expected));
                return;
            }
            other => panic!("expected hold status, got {other:?}"),
        }
    }
}

async fn recognize(shell: &mut Shell<tokio::io::DuplexStream>) {
    shell
        .send(&ControlToBrain::VoiceHoldRecognized {
            gesture: G,
            thread: None,
            selection: 4,
        })
        .await;
    status(shell, HoldStatus::Admitted).await;
}

async fn opened(shell: &mut Shell<tokio::io::DuplexStream>) {
    loop {
        match shell.control().await {
            Some(ControlToShell::Voice(_)) => {}
            Some(ControlToShell::Scenes { scenes }) => {
                assert!(scenes.is_empty());
                return;
            }
            other => panic!("expected initial scenes, got {other:?}"),
        }
    }
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the fixture supplies the protocol's last bit"
)]
async fn packet(
    shell: &mut Shell<tokio::io::DuplexStream>,
    rate: u32,
    seq: u32,
    last: bool,
    samples: Vec<i16>,
) {
    let message = DataToBrain::MicPcm(MicPcm {
        gesture: G,
        rate,
        seq,
        last,
        samples,
    });
    shell
        .data_out
        .write_all(&frame::encode(&message, Pipe::Data).unwrap())
        .await
        .unwrap();
}

async fn release_source(shell: &mut Shell<tokio::io::DuplexStream>, chunks: u32) {
    shell
        .send(&ControlToBrain::KeyUp {
            gesture: G,
            t_qpc: 2,
            chunks,
        })
        .await;
    shell
        .send(&ControlToBrain::VoiceHoldStopped { gesture: G })
        .await;
}

fn users(scratch: &Scratch) -> Vec<String> {
    let store = Store::open(&scratch.db()).unwrap();
    store
        .conn_for_tests()
        .prepare("SELECT text FROM events WHERE kind = 'user' ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn first_speech_setup_loads_and_holds_without_restarting_the_brain() {
    use kumochi_protocol::message::{PartState, SettingsOp};
    use kumochi_protocol::voice::VoicePart;

    let scratch = Scratch::trusted("hold-first-setup");
    let mut brain = test_support::brain(&scratch, "0 text first setup answer");
    let (release, held) = std::sync::mpsc::channel();
    let (began, mut decoded) = mpsc::unbounded_channel();
    let (reports, heard) = Reports::new();
    let (stt, _policy) = held_for_runtime_checked(reports, held, move |rate, samples| {
        assert_eq!(rate, 16_000);
        assert_eq!(samples, [73, -19]);
        began.send(()).unwrap();
        "first setup transcript".into()
    });
    let (_, tts) = Reports::new();
    let (fetch, fetched) = super::voice::held_fetch();
    brain.attach_voice(
        scratch.dir().join("models"),
        &[],
        (Some(stt), None),
        [heard, tts],
        Arc::<super::voice::HeldFetch>::clone(&fetch),
    );
    assert_eq!(brain.hold_voice().0, PartState::NotInstalled);
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opened(&mut shell).await;
        shell
            .send(&ControlToBrain::Settings(SettingsOp::InstallVoice(
                VoicePart::SpeechInput,
            )))
            .await;
        let Some(ControlToShell::Voice(view)) = shell.control().await else {
            panic!("initial install view");
        };
        assert_eq!(view.speech_input, PartState::Verifying);
        let Some(ControlToShell::Voice(view)) = shell.control().await else {
            panic!("download view");
        };
        assert_eq!(
            view.speech_input,
            PartState::Downloading { done: 3, total: 9 }
        );
        fetched.send(Ok(())).unwrap();
        let Some(ControlToShell::Voice(view)) = shell.control().await else {
            panic!("load view");
        };
        assert_eq!(view.speech_input, PartState::Loading);
        release.send(()).unwrap();
        let Some(ControlToShell::Voice(view)) = shell.control().await else {
            panic!("ready view after activation");
        };
        assert_eq!(view.speech_input, PartState::Ready);
        recognize(&mut shell).await;
        packet(&mut shell, 16_000, 0, true, vec![73, -19]).await;
        release_source(&mut shell, 1).await;
        status(&mut shell, HoldStatus::Transcribing).await;
        assert_eq!(decoded.recv().await, Some(()));
        release.send(()).unwrap();
        let (run, _) = shell.state().await;
        shell.read_to_end(run).await;
        drop(shell);
    };
    let (_, ()) = tokio::time::timeout(super::PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(brain.hold_voice().0, PartState::Ready);
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 1);
    brain.close().await;
    assert_eq!(users(&scratch), ["first setup transcript"]);
}

#[tokio::test]
async fn exact_ordered_pcm_and_independent_resampling_tail_reach_one_real_hold_run() {
    for controls_first in [false, true] {
        for resampled in [false, true] {
            let scratch = Scratch::new("hold-ordered-pcm");
            let mut expected = vec![713; 320];
            expected.extend([-93, 7, 301, -120]);
            if resampled {
                expected = vec![252, 194, 3, -145, -130, -13, 65, 49];
            }
            let (mut brain, release, mut decoded, fetch) =
                input(&scratch, expected, "recognized sentinel").await;
            let choice = ModelChoice {
                model: ModelSlug::new("voice-selected").unwrap(),
                effort: Effort::High,
                connection: ConnectionKind::ChatGpt,
            };
            assert!(
                Store::open(&scratch.db())
                    .unwrap()
                    .set_model(ThreadId(1), &choice)
                    .unwrap()
            );
            let (seen, mut requests) = mpsc::unbounded_channel();
            brain.engine = Box::new(Requested(seen));
            let ((control, data), mut shell) = duplex();
            let mut tap = Tap::from_env();
            let client = async {
                opened(&mut shell).await;
                shell
                    .send(&ControlToBrain::VoiceHoldRecognized {
                        gesture: G,
                        thread: Some(ThreadId(1)),
                        selection: 4,
                    })
                    .await;
                status(&mut shell, HoldStatus::Admitted).await;
                let chunks = if resampled { 1 } else { 2 };
                if controls_first {
                    release_source(&mut shell, chunks).await;
                }
                if resampled {
                    packet(&mut shell, 8_000, 0, true, vec![256, 0, -128, 64]).await;
                } else {
                    packet(&mut shell, 16_000, 0, false, vec![713; 320]).await;
                    packet(&mut shell, 16_000, 1, true, vec![-93, 7, 301, -120]).await;
                }
                if !controls_first {
                    release_source(&mut shell, chunks).await;
                }
                status(&mut shell, HoldStatus::Transcribing).await;
                assert_eq!(decoded.recv().await, Some(()));
                release.send(()).unwrap();
                let run = match shell.control().await {
                    Some(ControlToShell::RunState { run, origin, .. }) => {
                        assert_eq!(origin, Some(RunOrigin::Hold { gesture: G }));
                        run
                    }
                    other => panic!("expected authentic hold run, got {other:?}"),
                };
                assert_eq!(
                    shell.data().await,
                    DataToShell::AssistantText {
                        run,
                        delta: "selected answer".into()
                    }
                );
                shell.credit("selected answer".len() + 64).await;
                assert_eq!(shell.data().await, DataToShell::RunEnd { run });
                drop(shell);
            };
            let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
            assert_eq!(ended, Ended::Lost);
            assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
            let request = requests.try_recv().unwrap();
            assert_eq!(request.thread, ThreadId(1));
            assert_eq!(request.request.choice, Some(choice));
            let sent_users: Vec<_> = request
                .request
                .input
                .iter()
                .filter_map(|item| match item {
                    crate::context::InputItem::Message {
                        role: crate::context::Role::User,
                        text,
                    } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(sent_users.len(), 1);
            let context = sent_users[0]
                .strip_prefix("recognized sentinel\n\nLocal time: ")
                .unwrap();
            let (minute, drawings) = context.split_once("\nDrawings on screen: ").unwrap();
            super::asks::assert_minute(minute);
            assert_eq!(drawings, "[]");
            assert!(requests.is_empty());
            brain.close().await;
            assert_eq!(users(&scratch), ["recognized sentinel"]);
        }
    }
}

#[tokio::test]
async fn no_samples_or_blank_transcript_store_and_send_nothing() {
    for text in ["", " \t\r\n", "unused"] {
        let scratch = Scratch::new("hold-empty");
        let (mut brain, release, mut decoded, fetch) = input(&scratch, vec![91, -12], text).await;
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            opened(&mut shell).await;
            recognize(&mut shell).await;
            if text == "unused" {
                release_source(&mut shell, 0).await;
            } else {
                packet(&mut shell, 16_000, 0, true, vec![91, -12]).await;
                release_source(&mut shell, 1).await;
                status(&mut shell, HoldStatus::Transcribing).await;
                assert_eq!(decoded.recv().await, Some(()));
                release.send(()).unwrap();
            }
            status(&mut shell, HoldStatus::Empty).await;
            drop(shell);
        };
        let (_, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        assert_eq!(brain.coordinator.current_run(), None);
        assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
        brain.close().await;
        assert_eq!(users(&scratch), Vec::<String>::new());
    }
}

#[tokio::test]
async fn cancellation_and_new_explicit_input_fence_an_already_running_decode() {
    for typed in [false, true] {
        let scratch = Scratch::new("hold-late-decode");
        let (mut brain, release, mut decoded, _) = input(&scratch, vec![7, -90], "obsolete").await;
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            opened(&mut shell).await;
            recognize(&mut shell).await;
            packet(&mut shell, 16_000, 0, true, vec![7, -90]).await;
            release_source(&mut shell, 1).await;
            status(&mut shell, HoldStatus::Transcribing).await;
            assert_eq!(decoded.recv().await, Some(()));
            if typed {
                shell.submit("new explicit input").await;
            } else {
                shell
                    .send(&ControlToBrain::GestureCancel { gesture: G })
                    .await;
            }
            status(&mut shell, HoldStatus::Cancelled).await;
            release.send(()).unwrap();
            if typed {
                let (run, _) = shell.state().await;
                shell.read_to_end(run).await;
            }
            drop(shell);
        };
        let (_, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        brain.close().await;
        assert_eq!(
            users(&scratch),
            if typed {
                vec!["new explicit input".to_owned()]
            } else {
                Vec::new()
            }
        );
    }
}

#[tokio::test]
async fn a_failed_gapped_utterance_cannot_be_repaired_by_valid_late_frames() {
    let scratch = Scratch::new("hold-gap-terminal");
    let (mut brain, _release, mut decoded, _) =
        input(&scratch, Vec::new(), "must not decode").await;
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opened(&mut shell).await;
        recognize(&mut shell).await;
        packet(&mut shell, 16_000, 1, true, vec![9]).await;
        status(&mut shell, HoldStatus::Failed(HoldFailure::InvalidAudio)).await;
        packet(&mut shell, 16_000, 0, true, vec![9]).await;
        release_source(&mut shell, 1).await;
        shell.submit("only typed").await;
        let (run, _) = shell.state().await;
        shell.read_to_end(run).await;
        assert!(decoded.try_recv().is_err());
        drop(shell);
    };
    let (_, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    brain.close().await;
    assert_eq!(users(&scratch), ["only typed"]);
}

#[tokio::test]
async fn data_eof_fences_before_grace_and_only_the_old_authenticated_stop_clears_debt() {
    for stopped in [false, true] {
        let scratch = Scratch::new("hold-grace-debt");
        let (mut brain, _release, mut decoded, _) =
            input(&scratch, Vec::new(), "must not decode").await;
        let mut seen = brain.observe();
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            opened(&mut shell).await;
            recognize(&mut shell).await;
            shell.data_out.shutdown().await.unwrap();
            tokio::time::timeout(super::PATIENCE, async {
                while seen.recv().await != Some(crate::runtime::Seen::Grace) {}
            })
            .await
            .unwrap();
            if stopped {
                shell
                    .send(&ControlToBrain::VoiceHoldStopped { gesture: G })
                    .await;
            }
            shell
                .send(&ControlToBrain::VoiceHoldRecognized {
                    gesture: GestureId(18),
                    thread: None,
                    selection: 5,
                })
                .await;
            shell.control_out.shutdown().await.unwrap();
            drop(shell);
        };
        let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        assert_eq!(ended, Ended::Lost);
        let ((control, data), mut shell) = duplex();
        let client = async {
            opened(&mut shell).await;
            shell
                .send(&ControlToBrain::VoiceHoldRecognized {
                    gesture: G,
                    thread: None,
                    selection: 6,
                })
                .await;
            status(
                &mut shell,
                if stopped {
                    HoldStatus::Admitted
                } else {
                    HoldStatus::Failed(HoldFailure::Unavailable)
                },
            )
            .await;
            drop(shell);
        };
        let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        assert_eq!(ended, Ended::Lost);
        assert!(decoded.try_recv().is_err());
        brain.close().await;
        assert_eq!(users(&scratch), Vec::<String>::new());
    }
}

#[tokio::test]
async fn accepted_target_deletion_cancels_before_late_decode_and_uses_no_phantom_typed_count() {
    use kumochi_protocol::message::{Deleted, Phase};
    let scratch = Scratch::new("hold-deletion");
    let (mut brain, release, mut decoded, _) =
        input(&scratch, vec![8, -17], "deletable voice words").await;
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opened(&mut shell).await;
        recognize(&mut shell).await;
        packet(&mut shell, 16_000, 0, true, vec![8, -17]).await;
        release_source(&mut shell, 1).await;
        status(&mut shell, HoldStatus::Transcribing).await;
        assert_eq!(decoded.recv().await, Some(()));
        release.send(()).unwrap();
        let (run, _) = shell.state().await;
        let thread = shell.thread.unwrap();
        shell.read_to_end(run).await;
        while shell.state().await != (run, Phase::Done) {}
        let next = GestureId(18);
        shell
            .send(&ControlToBrain::VoiceHoldRecognized {
                gesture: next,
                thread: Some(thread),
                selection: 5,
            })
            .await;
        status_for(&mut shell, next, HoldStatus::Admitted).await;
        shell
            .send(&ControlToBrain::DeleteThread {
                thread,
                after_turns: 0,
                resend: false,
            })
            .await;
        status_for(
            &mut shell,
            next,
            HoldStatus::Failed(HoldFailure::ChoiceChanged),
        )
        .await;
        shell
            .send(&ControlToBrain::VoiceHoldStopped { gesture: next })
            .await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::ThreadDeleted {
                thread,
                outcome: Deleted::Removed,
                last_run: Some(run)
            })
        );
        shell
            .send(&ControlToBrain::KeyUp {
                gesture: next,
                t_qpc: 4,
                chunks: 0,
            })
            .await;
        let late = GestureId(19);
        shell
            .send(&ControlToBrain::VoiceHoldRecognized {
                gesture: late,
                thread: Some(thread),
                selection: 6,
            })
            .await;
        status_for(
            &mut shell,
            late,
            HoldStatus::Failed(HoldFailure::ChoiceChanged),
        )
        .await;
        assert!(decoded.try_recv().is_err());
        drop(shell);
    };
    let (_, ()) = tokio::time::timeout(super::PATIENCE * 2, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    brain.close().await;
    assert_eq!(users(&scratch), Vec::<String>::new());
}

#[tokio::test]
async fn loading_waits_only_for_the_same_held_gesture_and_never_fetches() {
    use kumochi_protocol::message::PartState;
    let scratch = Scratch::new("hold-loading-release");
    let (mut brain, release, fetch, unloaded) = super::voice::released_input(&scratch).await;
    brain.voice_event(unloaded, None).unwrap();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opened(&mut shell).await;
        shell
            .send(&ControlToBrain::KeyDown {
                gesture: G,
                t_qpc: 1,
            })
            .await;
        shell
            .send(&ControlToBrain::VoiceHoldRecognized {
                gesture: G,
                thread: None,
                selection: 1,
            })
            .await;
        status(&mut shell, HoldStatus::Loading).await;
        shell
            .send(&ControlToBrain::KeyUp {
                gesture: G,
                t_qpc: 2,
                chunks: 0,
            })
            .await;
        status(&mut shell, HoldStatus::Cancelled).await;
        release.send(()).unwrap();
        match shell.control().await {
            Some(ControlToShell::Voice(view)) if view.speech_input == PartState::Ready => {}
            other => panic!("expected completed local load, got {other:?}"),
        }
        let fresh = GestureId(18);
        shell
            .send(&ControlToBrain::VoiceHoldRecognized {
                gesture: fresh,
                thread: None,
                selection: 2,
            })
            .await;
        status_for(&mut shell, fresh, HoldStatus::Admitted).await;
        shell
            .send(&ControlToBrain::GestureCancel { gesture: fresh })
            .await;
        status_for(&mut shell, fresh, HoldStatus::Cancelled).await;
        shell
            .send(&ControlToBrain::VoiceHoldStopped { gesture: fresh })
            .await;
        drop(shell);
    };
    let (_, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
    brain.close().await;
    assert_eq!(users(&scratch), Vec::<String>::new());
}
