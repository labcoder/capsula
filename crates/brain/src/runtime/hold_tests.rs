use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    CaptureFor, ConnectionKind, ControlToBrain, ControlToShell, DataToBrain, DataToShell, Effort,
    GestureId, HoldFailure, HoldStatus, MicPcm, ModelChoice, ModelSlug, RunId, RunOrigin, ThreadId,
    ThreadOp,
};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;

use super::{Brain, Ended, Shell, duplex};
use crate::runtime::{Seen, voice};
use crate::store::Store;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};
use crate::voice::Reports;
use crate::voice::installer::{Durable, Installer};
use crate::voice::stt::{SttHandle, held_for_runtime_checked, pending_results, runtime_decoder};

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
            let crate::engines::Answer::Grant(mut lease) = gate.ask().await else {
                return;
            };
            crate::connections::model::Lease::wrote(&mut lease);
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

/// A `Requested` whose round reports its start, then waits for the test's word before it asks to
/// send.
struct Paced {
    started: mpsc::UnboundedSender<()>,
    go: Arc<tokio::sync::Notify>,
    seen: mpsc::UnboundedSender<crate::engines::RunRequest>,
}

impl std::fmt::Debug for Paced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Paced")
    }
}

impl crate::engines::Engine for Paced {
    fn start(
        &self,
        request: crate::engines::RunRequest,
        events: mpsc::Sender<crate::engines::TurnEvent>,
        gate: crate::engines::SendGate,
    ) -> crate::engines::Round {
        self.started.send(()).unwrap();
        let go = Arc::clone(&self.go);
        let round = Requested(self.seen.clone()).start(request, events, gate);
        Box::pin(async move {
            go.notified().await;
            round.await;
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
        began.send(()).expect("decode observed");
        assert_eq!(rate, 16_000);
        assert_eq!(samples, expected);
        text.into()
    });
    let fetch = install_input(&mut brain, scratch, stt, heard);
    (brain, release, heard_decode, fetch)
}

fn install_input(
    brain: &mut Brain,
    scratch: &Scratch,
    stt: SttHandle,
    heard: mpsc::UnboundedReceiver<crate::voice::LoadReport>,
) -> Arc<super::voice::HeldFetch> {
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
        true,
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
    fetch
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
    recognize_for(shell, G).await;
    status(shell, HoldStatus::Admitted).await;
}

async fn recognize_for(shell: &mut Shell<tokio::io::DuplexStream>, gesture: GestureId) {
    shell
        .send(&ControlToBrain::VoiceHoldRecognized {
            gesture,
            thread: None,
            selection: 4,
        })
        .await;
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
    packet_for(shell, G, rate, seq, last, samples).await;
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the fixture supplies the protocol's last bit for a selected gesture"
)]
async fn packet_for(
    shell: &mut Shell<tokio::io::DuplexStream>,
    gesture: GestureId,
    rate: u32,
    seq: u32,
    last: bool,
    samples: Vec<i16>,
) {
    let message = DataToBrain::MicPcm(MicPcm {
        gesture,
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
    release_source_for(shell, G, chunks).await;
}

async fn release_source_for(
    shell: &mut Shell<tokio::io::DuplexStream>,
    gesture: GestureId,
    chunks: u32,
) {
    shell
        .send(&ControlToBrain::KeyUp {
            gesture,
            t_qpc: 2,
            chunks,
        })
        .await;
    source_stopped(shell, gesture).await;
}

async fn source_stopped(shell: &mut Shell<tokio::io::DuplexStream>, expected: GestureId) {
    shell
        .send(&ControlToBrain::VoiceHoldStopped { gesture: expected })
        .await;
    loop {
        match shell.control().await {
            Some(ControlToShell::Voice(_)) => {}
            Some(ControlToShell::VoiceHoldStopAccepted { gesture }) => {
                assert_eq!(gesture, expected);
                return;
            }
            other => panic!("expected stop acceptance, got {other:?}"),
        }
    }
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

fn sent_users(request: &crate::engines::RunRequest) -> Vec<&str> {
    request
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
        .collect()
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
        assert_eq!(samples, [173, -19]);
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
        packet(&mut shell, 16_000, 0, true, vec![173, -19]).await;
        release_source(&mut shell, 1).await;
        status(&mut shell, HoldStatus::Transcribing).await;
        assert_eq!(decoded.recv().await, Some(()));
        release.send(()).unwrap();
        let (run, _) = shell.state().await;
        shell.held(G, "first setup transcript").await;
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
                shell.held(G, "recognized sentinel").await;
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
            let sent_users = sent_users(&request);
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
async fn accepted_sparse_capture_pcm_reaches_transcription() {
    let scratch = Scratch::trusted("hold-sparse-capture");
    let (mut brain, release, mut decoded, _fetch) =
        input(&scratch, vec![121, 31, 41, 51], "sparse capture transcript").await;
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opened(&mut shell).await;
        recognize(&mut shell).await;
        packet(&mut shell, 16_000, 0, true, vec![121, 31, 41, 51]).await;
        release_source(&mut shell, 1).await;
        status(&mut shell, HoldStatus::Transcribing).await;
        assert_eq!(decoded.recv().await, Some(()));
        release.send(()).unwrap();
        let (run, _) = shell.state().await;
        shell.held(G, "sparse capture transcript").await;
        shell.read_to_end(run).await;
        drop(shell);
    };
    let (_, ()) = tokio::time::timeout(super::PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    brain.close().await;
    assert_eq!(users(&scratch), ["sparse capture transcript"]);
}

#[tokio::test]
async fn a_screen_holds_words_wait_for_its_picture_in_the_running_brain() {
    use crate::runtime::hold::tests::screen_hold::{recognized, release as released, seeing};
    type Request = <crate::screen::Holds as crate::supervise::Kind>::Request;
    type Reply = <crate::screen::Holds as crate::supervise::Kind>::Reply;
    let scratch = Scratch::new("hold-screen-words");
    let (mut brain, release, mut decoded, _fetch) =
        input(&scratch, vec![105, -5], "words before the picture").await;
    let mut rig = seeing(&mut brain);
    let mut seen = brain.observe();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opened(&mut shell).await;
        shell.send(&recognized(G)).await;
        status(&mut shell, HoldStatus::Admitted).await;
        shell.send(&released(G, 100, 200, 2)).await;
        let (id, display) = rig.next().await;
        packet(&mut shell, 16_000, 0, true, vec![105, -5]).await;
        release_source(&mut shell, 1).await;
        status(&mut shell, HoldStatus::Transcribing).await;
        crate::screen::hold_frame(&rig, id, display).await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::CaptureDone {
                capture_for: CaptureFor::Gesture(G)
            })
        );
        let (id, seal) = rig.next().await;
        assert!(matches!(seal, Request::Seal(_)), "{seal:?}");
        assert_eq!(decoded.recv().await, Some(()));
        // The words come in while the seal is unanswered, and the signal shows they're parked to
        // wait for the picture before the seal is answered.
        release.send(()).unwrap();
        while !matches!(seen.recv().await, Some(Seen::WordsWaiting)) {}
        assert!(matches!(shell.data().await, DataToShell::Models { .. }));
        shell.held_words(G, "words before the picture").await;
        let refused = Reply::Refused(crate::screen::Refusal::DisplayChanged);
        rig.reply(id, &refused).await;
        let (id, retire) = rig.next().await;
        let Request::Retire(owner) = retire else {
            panic!("not a retire: {retire:?}")
        };
        rig.reply(id, &Reply::Released(owner, None)).await;
        let (run, _) = shell.state().await;
        shell.hold_stored(G).await;
        shell.read_to_end(run).await;
        drop(shell);
    };
    let (_, ()) = tokio::time::timeout(super::PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    brain.close().await;
    assert_eq!(users(&scratch), ["words before the picture"]);
}

/// The sealed picture of a screen hold.
const PICTURE: &[u8] = &[0xFF, 0xD8, 0xFF, 0xD9];

#[tokio::test]
async fn quiet_screen_holds_retire_pending_frames_and_discard_ready_images() {
    use crate::runtime::hold::tests::screen_hold::{recognized, release as released, seeing};
    type Request = <crate::screen::Holds as crate::supervise::Kind>::Request;
    type Reply = <crate::screen::Holds as crate::supervise::Kind>::Reply;
    for ready_image in [false, true] {
        let scratch = Scratch::new("hold-quiet-picture");
        let (mut brain, _release, mut decoded, _) =
            input(&scratch, Vec::new(), "must not decode").await;
        let mut rig = seeing(&mut brain);
        let mut observed = brain.observe();
        let owner = crate::coordinator::HoldKey {
            epoch: brain.epoch + 1,
            gesture: G,
        };
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
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
            if ready_image {
                rig.reply(id, &sealed).await;
                while !matches!(observed.recv().await, Some(Seen::PictureSealed)) {}
            }
            packet(&mut shell, 16_000, 0, true, vec![-103]).await;
            release_source(&mut shell, 1).await;
            status(&mut shell, HoldStatus::Empty).await;
            if !ready_image {
                rig.reply(id, &sealed).await;
                crate::screen::release_next(&mut rig, owner).await;
            }
            assert_eq!(decoded.try_recv(), Err(mpsc::error::TryRecvError::Empty));
            drop(shell);
        };
        tokio::time::timeout(super::PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .unwrap();
        assert_eq!(brain.coordinator.current_run(), None);
        brain.close().await;
        assert!(users(&scratch).is_empty());
    }
}

/// Which half of a screen hold the brain takes first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum First {
    Words,
    Picture,
}

/// Reads controls up to the hold's `HoldScreen`, which may come only after `go` lets the round
/// ask to send: an unarmed release is answered at once, so anything told at the run's start
/// arrives ahead of that answer.
async fn told_at_the_grant(shell: &mut Shell<tokio::io::DuplexStream>, go: &tokio::sync::Notify) {
    let other = GestureId(99);
    shell
        .send(&crate::runtime::hold::tests::screen_hold::release(
            other, 100, 200, 2,
        ))
        .await;
    let refused = ControlToShell::CaptureFailed {
        capture_for: CaptureFor::Gesture(other),
        reason: kumochi_protocol::message::CaptureFailure::Unavailable,
    };
    loop {
        match shell.control().await {
            Some(ControlToShell::RunState { .. }) => {}
            Some(message) if message == refused => break,
            other => panic!("expected the refused release, got {other:?}"),
        }
    }
    go.notify_one();
    let told = loop {
        match shell.control().await {
            Some(ControlToShell::RunState { .. }) => {}
            Some(message) => break message,
            None => panic!("the brain hung up"),
        }
    };
    assert_eq!(
        told,
        ControlToShell::HoldScreen {
            gesture: G,
            outcome: kumochi_protocol::message::HeldScreen::Screen
        }
    );
}

/// The run's history line says the picture went with it.
fn stored_with_its_picture(scratch: &Scratch, task: Option<ThreadId>) {
    let screens: Vec<Option<String>> = Store::open(&scratch.db())
        .unwrap()
        .conn_for_tests()
        .prepare("SELECT screen FROM runs ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut expected = vec![None; usize::from(task.is_some())];
    expected.push(Some("screen".to_owned()));
    assert_eq!(screens, expected);
}

fn seed_task_for_a_screen_hold(scratch: &Scratch) -> ThreadId {
    let mut store = Store::open(&scratch.db()).unwrap();
    let task = ThreadId(1);
    store
        .begin_turn(task, Some("Task"), RunId(1), "earlier words")
        .unwrap();
    store
        .end_run(task, RunId(1), &[], crate::store::RunEnd::Done)
        .unwrap();
    task
}

async fn commit_hold_model(
    shell: &mut Shell<tokio::io::DuplexStream>,
    thread: ThreadId,
    choice: &ModelChoice,
) {
    shell
        .send(&ControlToBrain::ThreadOp(ThreadOp::SetModel {
            thread,
            choice: choice.clone(),
        }))
        .await;
    assert_eq!(
        shell.control().await,
        Some(ControlToShell::ThreadModelSet {
            thread,
            choice: choice.clone(),
            applied: true,
        })
    );
}

async fn recognize_screen_recipient(
    shell: &mut Shell<tokio::io::DuplexStream>,
    task: Option<ThreadId>,
    choice: &ModelChoice,
) {
    opened(shell).await;
    if let Some(thread) = task {
        commit_hold_model(shell, thread, choice).await;
    }
    shell
        .send(&ControlToBrain::ScreenHoldRecognized {
            gesture: G,
            thread: task,
            selection: 1,
            t_qpc: 1,
            notice: Some(kumochi_protocol::message::NoticeRevision::FIRST),
        })
        .await;
}

/// A spoken screen hold through the running brain whose picture is sealed, its words coming
/// before the picture or after it: the run carries both, and `HoldScreen` waits for the grant.
async fn a_screen_hold_reaches_its_run_with_its_picture(first: First) {
    use crate::runtime::hold::tests::screen_hold::{release as released, seeing};
    type Request = <crate::screen::Holds as crate::supervise::Kind>::Request;
    type Reply = <crate::screen::Holds as crate::supervise::Kind>::Reply;
    let scratch = Scratch::new("hold-screen-picture");
    let task = (first == First::Picture).then(|| seed_task_for_a_screen_hold(&scratch));
    let choice = ModelChoice {
        model: ModelSlug::new("gpt-6-sol").unwrap(),
        effort: if task.is_some() {
            Effort::High
        } else {
            Effort::Low
        },
        connection: ConnectionKind::ChatGpt,
    };
    let (mut brain, release, mut decoded, _fetch) =
        input(&scratch, vec![105, -5], "words and a picture").await;
    let mut rig = seeing(&mut brain);
    brain.fixture = Some(crate::coordinator::Ready {
        images: vec![kumochi_protocol::message::ModelSlug::new("gpt-6-sol").unwrap()],
        ..crate::coordinator::Ready::fixture()
    });
    let mut observed = brain.observe();
    let (seen, mut requests) = mpsc::unbounded_channel();
    let (started, mut round) = mpsc::unbounded_channel();
    let go = Arc::new(tokio::sync::Notify::new());
    brain.engine = Box::new(Paced {
        started,
        go: Arc::clone(&go),
        seen,
    });
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        recognize_screen_recipient(&mut shell, task, &choice).await;
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
        if first == First::Picture {
            rig.reply(id, &sealed).await;
            while !matches!(observed.recv().await, Some(Seen::PictureSealed)) {}
        }
        packet(&mut shell, 16_000, 0, true, vec![105, -5]).await;
        release_source(&mut shell, 1).await;
        status(&mut shell, HoldStatus::Transcribing).await;
        assert_eq!(decoded.recv().await, Some(()));
        release.send(()).unwrap();
        if first == First::Words {
            while !matches!(observed.recv().await, Some(Seen::WordsWaiting)) {}
            rig.reply(id, &sealed).await;
        }
        let (run, _) = shell.state().await;
        assert_eq!(round.recv().await, Some(()));
        told_at_the_grant(&mut shell, &go).await;
        assert!(matches!(shell.data().await, DataToShell::Models { .. }));
        shell.held(G, "words and a picture").await;
        shell.read_to_end(run).await;
        drop(shell);
    };
    let (_, ()) = tokio::time::timeout(super::PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    let request = requests.try_recv().unwrap();
    assert_eq!(request.request.choice, Some(choice));
    if let Some(task) = task {
        assert_eq!(request.thread, task);
    }
    let jpeg = crate::context::Jpeg::new(PICTURE.to_vec()).unwrap();
    let images: Vec<_> = request
        .request
        .input
        .iter()
        .filter_map(|item| match item {
            crate::context::InputItem::Image(image) => Some(image),
            _ => None,
        })
        .collect();
    assert_eq!(images, [&jpeg]);
    assert!(requests.is_empty());
    brain.close().await;
    let mut expected = vec!["earlier words"; usize::from(task.is_some())];
    expected.push("words and a picture");
    assert_eq!(users(&scratch), expected);
    stored_with_its_picture(&scratch, task);
}

#[tokio::test]
async fn a_screen_holds_words_then_its_picture_reach_the_run_together() {
    Box::pin(a_screen_hold_reaches_its_run_with_its_picture(First::Words)).await;
}

#[tokio::test]
async fn a_screen_holds_picture_then_its_words_reach_the_run_together() {
    Box::pin(a_screen_hold_reaches_its_run_with_its_picture(
        First::Picture,
    ))
    .await;
}

#[path = "screen_sent_tests.rs"]
mod sent;

#[tokio::test]
async fn a_voice_hold_uses_the_committed_task_model_and_effort() {
    use crate::context::InputItem;
    use crate::runtime::hold::tests::screen_hold::image_recipients;
    use kumochi_protocol::message::Phase;

    let scratch = Scratch::new("hold-voice-pin");
    let task = seed_task_for_a_screen_hold(&scratch);
    let (mut brain, release, mut decoded, _fetch) =
        input(&scratch, vec![105, -5], "new spoken words").await;
    let (_, choice) = image_recipients(&mut brain);
    assert!(brain.account.takes_images(&choice));
    let (send, mut requests) = mpsc::unbounded_channel();
    brain.engine = Box::new(Requested(send));
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opened(&mut shell).await;
        commit_hold_model(&mut shell, task, &choice).await;
        shell
            .send(&ControlToBrain::VoiceHoldRecognized {
                gesture: G,
                thread: Some(task),
                selection: 1,
            })
            .await;
        status(&mut shell, HoldStatus::Admitted).await;
        packet(&mut shell, 16_000, 0, true, vec![105, -5]).await;
        release_source(&mut shell, 1).await;
        status(&mut shell, HoldStatus::Transcribing).await;
        assert_eq!(decoded.recv().await, Some(()));
        release.send(()).unwrap();
        let (run, phase) = shell.state().await;
        assert_eq!(phase, Phase::Thinking);
        assert!(matches!(shell.data().await, DataToShell::Models { .. }));
        shell.held(G, "new spoken words").await;
        assert_eq!(shell.read_to_end(run).await, "selected answer".len());
        assert_eq!(shell.state().await, (run, Phase::Answering));
        assert_eq!(shell.state().await, (run, Phase::Done));
        drop(shell);
    };
    let (ended, ()) = tokio::time::timeout(super::PATIENCE, async {
        tokio::join!(brain.serve(control, data, &mut tap), client)
    })
    .await
    .unwrap();
    assert_eq!(ended, Ended::Lost);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.thread, task);
    assert_eq!(request.request.choice, Some(choice));
    assert!(
        !request
            .request
            .input
            .iter()
            .any(|item| matches!(item, InputItem::Image(_)))
    );
    let words = sent_users(&request);
    let [earlier, current] = words.as_slice() else {
        panic!("expected two user turns");
    };
    assert_eq!(*earlier, "earlier words");
    let (minute, drawings) = current
        .strip_prefix("new spoken words\n\nLocal time: ")
        .unwrap()
        .split_once("\nDrawings on screen: ")
        .unwrap();
    super::asks::assert_minute(minute);
    assert_eq!(drawings, "[]");
    assert!(requests.is_empty());
    brain.close().await;
    assert_eq!(users(&scratch), ["earlier words", "new spoken words"]);
}

#[tokio::test]
async fn quiet_audio_or_blank_transcript_store_and_send_nothing() {
    for (samples, text, controls_first) in [
        (Vec::new(), "unused", false),
        (vec![103, -103], "unused", false),
        (vec![103, -103], "unused", true),
        (vec![191, -12], "", false),
        (vec![191, -12], " \t\r\n", false),
    ] {
        let scratch = Scratch::new("hold-empty");
        let expected = if text == "unused" {
            Vec::new()
        } else {
            samples.clone()
        };
        let (mut brain, release, mut decoded, fetch) = input(&scratch, expected, text).await;
        let (started, mut starts) = mpsc::unbounded_channel();
        brain.engine = Box::new(Paced {
            started,
            go: Arc::default(),
            seen: mpsc::unbounded_channel().0,
        });
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            opened(&mut shell).await;
            recognize(&mut shell).await;
            let chunks = u32::from(!samples.is_empty());
            if controls_first {
                release_source(&mut shell, chunks).await;
            }
            if chunks != 0 {
                packet(&mut shell, 16_000, 0, true, samples).await;
            }
            if !controls_first {
                release_source(&mut shell, chunks).await;
            }
            if text != "unused" {
                status(&mut shell, HoldStatus::Transcribing).await;
                assert_eq!(decoded.recv().await, Some(()));
                release.send(()).unwrap();
            }
            status(&mut shell, HoldStatus::Empty).await;
            packet(&mut shell, 16_000, 0, true, vec![200]).await;
            source_stopped(&mut shell, G).await;
            assert_eq!(decoded.try_recv(), Err(mpsc::error::TryRecvError::Empty));
            drop(shell);
        };
        tokio::time::timeout(super::PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .unwrap();
        assert_eq!(starts.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        assert_eq!(brain.coordinator.current_run(), None);
        assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
        brain.close().await;
        assert_eq!(users(&scratch), Vec::<String>::new());
    }
}

#[tokio::test]
async fn a_fresh_hold_uses_only_its_own_decode_in_either_completion_order() {
    const REFUSED: GestureId = GestureId(G.0 + 1);
    const FRESH: GestureId = GestureId(G.0 + 2);
    for stale_first in [false, true] {
        let scratch = Scratch::new("hold-overlapping-decodes");
        let mut brain = test_support::brain(&scratch, "0 text selected answer");
        let (seen, mut requests) = mpsc::unbounded_channel();
        brain.engine = Box::new(Requested(seen));
        let (reports, heard) = Reports::new();
        let (load, loaded) = std::sync::mpsc::channel();
        let (stale, wait_stale) = std::sync::mpsc::channel();
        let (fresh, wait_fresh) = std::sync::mpsc::channel();
        let gates = Mutex::new([Some(wait_stale), Some(wait_fresh)]);
        let (started, mut starts) = mpsc::unbounded_channel();
        let (stt, _) = runtime_decoder(reports, loaded, move |_, rate, samples| {
            assert_eq!(rate, 16_000);
            let (slot, words) = match samples {
                [201] => (0, "obsolete words"),
                [202] => (1, "fresh words"),
                _ => panic!("unexpected fixture PCM"),
            };
            let gate = gates.lock().unwrap()[slot].take().unwrap();
            started.send(slot).unwrap();
            gate.recv_timeout(super::PATIENCE).unwrap();
            words.into()
        });
        let pending = pending_results(&stt);
        let fetch = install_input(&mut brain, &scratch, stt, heard);
        load.send(()).unwrap();
        let event = tokio::time::timeout(super::PATIENCE, voice::next(&mut brain.voice))
            .await
            .unwrap();
        brain.voice_event(event, None).unwrap();
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            opened(&mut shell).await;
            recognize(&mut shell).await;
            packet(&mut shell, 16_000, 0, true, vec![201]).await;
            release_source(&mut shell, 1).await;
            status(&mut shell, HoldStatus::Transcribing).await;
            assert_eq!(starts.recv().await, Some(0));
            recognize_for(&mut shell, REFUSED).await;
            status_for(&mut shell, REFUSED, HoldStatus::Failed(HoldFailure::Busy)).await;
            shell
                .send(&ControlToBrain::GestureCancel { gesture: G })
                .await;
            status(&mut shell, HoldStatus::Cancelled).await;
            recognize_for(&mut shell, REFUSED).await;
            recognize_for(&mut shell, FRESH).await;
            status_for(&mut shell, FRESH, HoldStatus::Admitted).await;
            packet_for(&mut shell, FRESH, 16_000, 0, true, vec![202]).await;
            release_source_for(&mut shell, FRESH, 1).await;
            status_for(&mut shell, FRESH, HoldStatus::Transcribing).await;
            assert_eq!(starts.recv().await, Some(1));
            assert_eq!(pending(), 2);
            if stale_first {
                stale.send(()).unwrap();
                while pending() == 2 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(pending(), 1);
                assert!(matches!(
                    requests.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
            }
            fresh.send(()).unwrap();
            let (run, _) = shell.state().await;
            shell.held(FRESH, "fresh words").await;
            shell.read_to_end(run).await;
            if !stale_first {
                assert_eq!(pending(), 1);
                stale.send(()).unwrap();
            }
            while pending() != 0 {
                tokio::task::yield_now().await;
            }
            let request = requests.try_recv().unwrap();
            assert!(
                matches!(sent_users(&request).as_slice(), [words] if words.starts_with("fresh words\n\nLocal time: "))
            );
            assert!(requests.is_empty());
            drop(shell);
        };
        let (ended, ()) = tokio::time::timeout(super::PATIENCE, async {
            tokio::join!(brain.serve(control, data, &mut tap), client)
        })
        .await
        .unwrap();
        assert_eq!(ended, Ended::Lost);
        assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
        brain.close().await;
        assert_eq!(users(&scratch), ["fresh words"]);
    }
}

#[tokio::test]
async fn cancellation_and_new_explicit_input_fence_an_already_running_decode() {
    for typed in [false, true] {
        let scratch = Scratch::new("hold-late-decode");
        let (mut brain, release, mut decoded, _) = input(&scratch, vec![7, -190], "obsolete").await;
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            opened(&mut shell).await;
            recognize(&mut shell).await;
            packet(&mut shell, 16_000, 0, true, vec![7, -190]).await;
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
        packet(&mut shell, 16_000, 0, true, vec![109]).await;
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
async fn held_words_go_out_up_to_their_bound_and_are_never_cut() {
    use kumochi_protocol::message::MAX_TRANSCRIPT_BYTES;
    for len in [MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_BYTES + 1] {
        let words: &'static str = Box::leak("w".repeat(len).into_boxed_str());
        let scratch = Scratch::new("hold-words-bound");
        let (mut brain, release, mut decoded, _) = input(&scratch, vec![108, -17], words).await;
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            opened(&mut shell).await;
            recognize(&mut shell).await;
            packet(&mut shell, 16_000, 0, true, vec![108, -17]).await;
            release_source(&mut shell, 1).await;
            status(&mut shell, HoldStatus::Transcribing).await;
            assert_eq!(decoded.recv().await, Some(()));
            release.send(()).unwrap();
            let (run, _) = shell.state().await;
            if len <= MAX_TRANSCRIPT_BYTES {
                shell.held_words(G, words).await;
            }
            shell.hold_stored(G).await;
            shell.read_to_end(run).await;
            drop(shell);
        };
        let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        assert_eq!(ended, Ended::Lost);
        brain.close().await;
        assert_eq!(users(&scratch), [words], "the turn keeps every word");
    }
}

#[tokio::test]
async fn a_held_receipt_waits_for_the_commit_and_never_reaches_a_later_connection() {
    use kumochi_protocol::message::Phase;
    for lost in [false, true] {
        let scratch = Scratch::new("hold-receipt");
        let (mut brain, release, mut decoded, _) =
            input(&scratch, vec![108, -17], "receipt words").await;
        let commit =
            brain.hold_store(|op| matches!(op, crate::coordinator::StoreOp::BeginTurn { .. }));
        let ((control, data), mut shell) = duplex();
        let mut tap = Tap::from_env();
        let client = async {
            opened(&mut shell).await;
            recognize(&mut shell).await;
            packet(&mut shell, 16_000, 0, true, vec![108, -17]).await;
            release_source(&mut shell, 1).await;
            status(&mut shell, HoldStatus::Transcribing).await;
            assert_eq!(decoded.recv().await, Some(()));
            release.send(()).unwrap();
            shell.held_words(G, "receipt words").await;
            let (run, phase) = shell.state().await;
            assert_eq!(phase, Phase::Thinking);
            let early = tokio::time::timeout(std::time::Duration::from_millis(200), shell.data());
            assert!(early.await.is_err(), "no receipt before the commit");
            if !lost {
                commit.send(()).unwrap();
                shell.hold_stored(G).await;
                shell.read_to_end(run).await;
            }
            drop(shell);
        };
        let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
        assert_eq!(ended, Ended::Lost);
        if lost {
            // The old connection's commit lands now; the next turn's answer comes after it.
            commit.send(()).unwrap();
            let ((control, data), mut shell) = duplex();
            let client = async {
                opened(&mut shell).await;
                shell.submit("typed after the loss").await;
                let (run, _) = shell.state().await;
                shell.read_to_end(run).await;
                drop(shell);
            };
            let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
            assert_eq!(ended, Ended::Lost);
        }
        brain.close().await;
    }
}

#[tokio::test]
async fn accepted_target_deletion_cancels_before_late_decode_and_uses_no_phantom_typed_count() {
    use kumochi_protocol::message::{Deleted, Phase};
    let scratch = Scratch::new("hold-deletion");
    let (mut brain, release, mut decoded, _) =
        input(&scratch, vec![108, -17], "deletable voice words").await;
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        opened(&mut shell).await;
        recognize(&mut shell).await;
        packet(&mut shell, 16_000, 0, true, vec![108, -17]).await;
        release_source(&mut shell, 1).await;
        status(&mut shell, HoldStatus::Transcribing).await;
        assert_eq!(decoded.recv().await, Some(()));
        release.send(()).unwrap();
        let (run, _) = shell.state().await;
        let thread = shell.thread.unwrap();
        shell.held(G, "deletable voice words").await;
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
        let mut replies = vec![shell.control().await, shell.control().await];
        replies.sort_by_key(|reply| {
            matches!(reply, Some(ControlToShell::VoiceHoldStopAccepted { .. }))
        });
        assert_eq!(
            replies,
            [
                Some(ControlToShell::ThreadDeleted {
                    thread,
                    outcome: Deleted::Removed,
                    last_run: Some(run)
                }),
                Some(ControlToShell::VoiceHoldStopAccepted { gesture: next })
            ]
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
        source_stopped(&mut shell, fresh).await;
        drop(shell);
    };
    let (_, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(fetch.begun.load(Ordering::SeqCst), 0);
    brain.close().await;
    assert_eq!(users(&scratch), Vec::<String>::new());
}

#[tokio::test]
async fn a_replayed_receipt_from_the_same_live_shell_admits_the_next_hold() {
    use crate::platform::ShellProcess;
    use std::sync::atomic::AtomicU8;
    let scratch = Scratch::new("hold-replay-admits");
    let (mut brain, _release, _decoded, _) = input(&scratch, Vec::new(), "must not decode").await;
    let live = Arc::new(AtomicU8::new(0));
    let mut tap = Tap::from_env();
    brain.connected_shell(ShellProcess::fixture(10, Arc::clone(&live)));
    let ((control, data), mut shell) = duplex();
    let client = async {
        opened(&mut shell).await;
        recognize(&mut shell).await;
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.connected_shell(ShellProcess::fixture(10, Arc::clone(&live)));
    let ((control, data), mut shell) = duplex();
    let client = async {
        opened(&mut shell).await;
        shell
            .send(&ControlToBrain::VoiceHoldRecognized {
                gesture: G,
                thread: None,
                selection: 5,
            })
            .await;
        status(&mut shell, HoldStatus::Failed(HoldFailure::Unavailable)).await;
        source_stopped(&mut shell, G).await;
        shell
            .send(&ControlToBrain::VoiceHoldRecognized {
                gesture: GestureId(18),
                thread: None,
                selection: 6,
            })
            .await;
        status_for(&mut shell, GestureId(18), HoldStatus::Admitted).await;
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

/// Control frames the brain sends within `window`, past voice, settings and connection state.
async fn sent_within(
    shell: &mut Shell<tokio::io::DuplexStream>,
    window: std::time::Duration,
) -> Vec<ControlToShell> {
    let until = tokio::time::Instant::now() + window;
    let mut sent = Vec::new();
    while let Ok(frame) = tokio::time::timeout_at(until, shell.next_control()).await {
        match frame.expect("readable") {
            Some(
                ControlToShell::Voice(_)
                | ControlToShell::Settings { .. }
                | ControlToShell::Account(_)
                | ControlToShell::Connections(_)
                | ControlToShell::ScreenHolds(_),
            ) => {}
            Some(other) => sent.push(other),
            None => break,
        }
    }
    sent
}

/// A replayed receipt whose shell process can't be read, on an idle connection with no run and so
/// no ping, is checked again on the connection's own tick: nothing goes while the process can't be
/// read, one acceptance once it can, the next hold is admitted, and no tick work is left.
#[tokio::test]
async fn an_unreadable_replayed_receipt_is_accepted_on_the_idle_connections_tick() {
    use crate::platform::ShellProcess;
    use std::sync::atomic::AtomicU8;
    let window = std::time::Duration::from_millis(1200);
    let scratch = Scratch::new("hold-replay-unreadable-idle");
    let (mut brain, _release, _decoded, _) = input(&scratch, Vec::new(), "must not decode").await;
    let live = Arc::new(AtomicU8::new(0));
    let mut tap = Tap::from_env();
    brain.connected_shell(ShellProcess::fixture(10, Arc::clone(&live)));
    let ((control, data), mut shell) = duplex();
    let client = async {
        opened(&mut shell).await;
        recognize(&mut shell).await;
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.connected_shell(ShellProcess::fixture(10, Arc::clone(&live)));
    let ((control, data), mut shell) = duplex();
    let client = async {
        opened(&mut shell).await;
        live.store(2, Ordering::Release);
        shell
            .send(&ControlToBrain::VoiceHoldStopped { gesture: G })
            .await;
        assert_eq!(
            sent_within(&mut shell, window).await,
            [],
            "nothing while the process can't be read"
        );
        live.store(0, Ordering::Release);
        assert_eq!(
            sent_within(&mut shell, window).await,
            [ControlToShell::VoiceHoldStopAccepted { gesture: G }],
            "one acceptance, and no ping"
        );
        recognize_for(&mut shell, GestureId(18)).await;
        status_for(&mut shell, GestureId(18), HoldStatus::Admitted).await;
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    assert!(!brain.owes_stop(), "no tick work is left");
    brain.close().await;
}
