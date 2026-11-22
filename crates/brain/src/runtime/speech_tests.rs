//! A spoken run through the core loop: TTS audio reaches the shell through the pump, and the run
//! ends `Done` once the shell played it.

use std::path::Path;
use std::time::Duration;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    AudioSession, ControlToBrain, ControlToShell, DataToBrain, DataToShell, OutputGeneration,
    PageOf, Phase, RunId, SegmentId, SegmentStart, SpeechDecision, SpeechDispatch, SpeechPcm,
    SpeechPromise, SpeechRetirement, StepRef, ThreadId,
};
use tokio::io::AsyncWriteExt as _;
use tokio::time::{sleep, timeout};

use super::{Brain, Ended, PATIENCE, duplex};
use crate::coordinator::{Effect, Input, SendKey, StoreOp};
use crate::engines::fixture::Fixture;
use crate::engines::{self, Finish, Round, RunRequest, SendGate, TurnEvent};
use crate::store::{RunEnd, Store};
use crate::tap::Tap;
use crate::test_support::{self, Scratch};
use crate::voice::pump::{OutputMode, Pump};
use crate::voice::reply::Heard;
use crate::voice::tts::{Engine, TtsError, TtsHandle};

#[derive(Debug)]
struct DrawThenFinish(Fixture);

impl engines::Engine for DrawThenFinish {
    fn start(
        &self,
        request: RunRequest,
        events: tokio::sync::mpsc::Sender<TurnEvent>,
        gate: SendGate,
    ) -> Round {
        let SendKey::Build(key) = request.key else {
            panic!("a foreground run");
        };
        if key.round == 1 {
            return engines::Engine::start(&self.0, request, events, gate);
        }
        assert_eq!(key.round, 2);
        Box::pin(async move {
            events
                .send(TurnEvent::Finished(Finish::Completed))
                .await
                .unwrap();
        })
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

/// A brain playing `script` with spoken replies on, speaking through `Silence`.
fn spoken(scratch: &Scratch, script: &str) -> Brain {
    let mut brain = test_support::brain(scratch, script);
    enable_spoken(&mut brain, OutputMode::Legacy);
    brain
}

fn enable_spoken(brain: &mut Brain, output: OutputMode) {
    let (reports, _) = crate::voice::Reports::new();
    let tts = TtsHandle::spawn_with(tokio::runtime::Handle::current(), reports, |_: &Path| {
        Ok(Silence)
    })
    .expect("the owner thread starts");
    tts.load("models".into(), 1);
    brain.pump = Some(Pump::new(tts, output));
    brain.offline(Input::SpokenReplies { loaded: true });
    brain.offline(Input::AudioOutput {
        rate: 24_000,
        generation: OutputGeneration(1),
    });
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered exchange over the two real pipes"
)]
async fn closed_speech_waits_across_live_data_credits_then_dispatches_once() {
    use super::super::draw_stream_tests;
    use crate::coordinator::DRAIN_STALL;

    let scratch = Scratch::new("queued-speech-decisions");
    let history = ThreadId(2);
    {
        let mut store = Store::open(&scratch.db()).unwrap();
        store
            .begin_turn(history, Some("history"), RunId(1), &"x".repeat(31 * 1024))
            .unwrap();
        store.end_run(history, RunId(1), &[], RunEnd::Done).unwrap();
    }
    let mut brain = draw_stream_tests::brain(&scratch);
    let script = concat!(
        "0 text Plain.\n",
        r#"0 step {"records":[{"begin":{"v":1,"scene":"diagram","surface":"overlay","alt":""}},"#,
        r#"{"step":{"id":"s1","start":"speech","say":"Narration.","put":[{"shape":{"id":"a","#,
        r#""kind":"rect","box":[0,0,10,10]}}]}},{"end":{}}]}"#,
    );
    brain.engine = Box::new(DrawThenFinish(Fixture::parse(script).unwrap()));
    // This shell conducts: it decides each speech-start segment.
    enable_spoken(&mut brain, OutputMode::Conducted);
    let inputs = brain.inputs_tx.clone();
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        assert_eq!(shell.scenes().await, []);
        inputs.send(draw_stream_tests::provider_ready()).unwrap();
        let output = OutputGeneration(7);
        shell
            .send(&ControlToBrain::QueuedAudioOutput(24_000, output))
            .await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::QueuedAudioRegistered(output))
        );
        shell.submit("draw").await;
        let (run, phase) = shell.state().await;
        assert_eq!(phase, Phase::Thinking);
        assert_eq!(shell.state().await, (run, Phase::Answering));
        let key = SpeechDispatch {
            run,
            output,
            segment: SegmentId(1),
        };
        let retire = |segment, samples| {
            ControlToBrain::SpeechRetired(SpeechRetirement {
                run,
                output,
                segment,
                retired: samples,
                heard: samples,
                final_segment: true,
            })
        };
        let pcm = |segment, seq, samples, last| {
            DataToShell::SpeechPcm(SpeechPcm {
                run,
                segment,
                output_generation: output,
                seq,
                samples,
                last,
            })
        };
        let (mut scene, mut committed, mut announced, mut scene_ended) =
            (None, false, false, false);
        let mut plain_pcm = Vec::new();
        while !(announced && scene_ended && plain_pcm.len() == 2) {
            let frame = shell.data().await;
            shell
                .credit(frame::encode(&frame, Pipe::Data).unwrap().len())
                .await;
            match frame {
                DataToShell::AssistantText { run: owner, delta } => {
                    assert_eq!((owner, delta.as_str()), (run, "Plain."));
                }
                DataToShell::SceneBegin { scene: id, .. } => assert_eq!(scene.replace(id), None),
                DataToShell::SegmentQueued {
                    run: owner,
                    segment: SegmentId(0),
                    step,
                    start,
                    output_generation,
                } => {
                    assert_eq!(
                        (owner, step, start, output_generation),
                        (run, None, SegmentStart::Plain, output)
                    );
                }
                DataToShell::SceneStepCommit {
                    run: owner,
                    scene: id,
                    index,
                    speech,
                    ..
                } => {
                    assert_eq!(
                        (owner, index, speech),
                        (
                            run,
                            0,
                            Some(SpeechPromise {
                                output_generation: output,
                                segment: key.segment
                            })
                        )
                    );
                    assert_eq!(scene, Some(id));
                    assert!(!std::mem::replace(&mut committed, true));
                }
                DataToShell::SegmentQueued {
                    run: owner,
                    segment,
                    step,
                    start,
                    output_generation,
                } => {
                    assert!(committed);
                    let id = scene.expect("commit precedes its speech");
                    assert_eq!(
                        (owner, segment, step, start, output_generation),
                        (
                            run,
                            key.segment,
                            Some(StepRef {
                                scene: id,
                                index: 0
                            }),
                            SegmentStart::Speech,
                            output
                        )
                    );
                    assert!(!std::mem::replace(&mut announced, true));
                }
                DataToShell::SceneEnd {
                    run: owner,
                    scene: id,
                    complete,
                } => {
                    assert_eq!((owner, Some(id), complete), (run, scene, true));
                    assert!(!std::mem::replace(&mut scene_ended, true));
                }
                DataToShell::SpeechPcm(SpeechPcm {
                    segment: SegmentId(0),
                    ..
                }) => plain_pcm.push(frame),
                other => panic!("unexpected pre-decision data: {other:?}"),
            }
        }
        assert_eq!(
            plain_pcm,
            [
                pcm(SegmentId(0), 0, vec![0; 240], None),
                pcm(SegmentId(0), 1, Vec::new(), Some(240))
            ]
        );
        shell.send(&retire(SegmentId(0), 240)).await;

        let waiting = tokio::time::Instant::now();
        let mut returned = 0;
        for request in 1..=18 {
            if request % 3 == 1 {
                sleep(Duration::from_secs(1)).await;
            }
            let data = DataToBrain::PageRequest {
                request,
                page: PageOf::Events {
                    thread: history,
                    before: None,
                },
            };
            shell
                .data_out
                .write_all(&frame::encode(&data, Pipe::Data).unwrap())
                .await
                .unwrap();
            let frame = shell.data().await;
            assert!(
                matches!(&frame, DataToShell::Page { request: reply, thread, events, before: None }
                    if *reply == request && *thread == history && events.len() == 1),
                "{frame:?}"
            );
            let bytes = frame::encode(&frame, Pipe::Data).unwrap().len();
            shell.credit(bytes).await;
            returned += bytes;
        }
        assert!(returned > super::super::window());
        assert!(waiting.elapsed() > DRAIN_STALL);
        shell
            .send(&ControlToBrain::SpeechDecision(key, SpeechDecision::Allow))
            .await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::SpeechDecisionTaken(key))
        );
        shell
            .send(&ControlToBrain::SpeechDecision(key, SpeechDecision::Allow))
            .await;
        assert_eq!(
            shell.control().await,
            Some(ControlToShell::SpeechDecisionTaken(key))
        );
        assert_eq!(shell.data().await, pcm(key.segment, 0, vec![0; 240], None));
        assert_eq!(
            shell.data().await,
            pcm(key.segment, 1, Vec::new(), Some(240))
        );
        shell.send(&retire(key.segment, 240)).await;
        assert_eq!(shell.state().await, (run, Phase::Done));
        assert_eq!(shell.data().await, DataToShell::RunEnd { run });
        drop(shell);
    };
    let (ended, ()) = timeout(PATIENCE, async {
        tokio::join!(
            brain.serve_with_audio(control, data, &mut tap, AudioSession::QueuedRequired),
            client
        )
    })
    .await
    .expect("connected speech run finishes");
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

#[tokio::test]
async fn a_spoken_answer_streams_through_the_pump_and_ends_done_once_played() {
    let scratch = Scratch::new("spoken");
    let mut brain = spoken(&scratch, "0 text Hello there.");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        assert_eq!(shell.state().await, (run, Phase::Answering));
        let segment = SegmentId(0);
        let pcm = |seq, samples, last| {
            DataToShell::SpeechPcm(SpeechPcm {
                run,
                segment,
                output_generation: OutputGeneration(1),
                seq,
                samples,
                last,
            })
        };
        let mut frames = Vec::new();
        for _ in 0..4 {
            frames.push(shell.data().await);
        }
        assert_eq!(
            frames,
            [
                DataToShell::AssistantText {
                    run,
                    delta: "Hello there.".into(),
                },
                DataToShell::SegmentQueued {
                    run,
                    segment,
                    step: None,
                    start: kumochi_protocol::message::SegmentStart::Plain,
                    output_generation: kumochi_protocol::message::OutputGeneration(1),
                },
                pcm(0, vec![0; 240], None),
                pcm(1, Vec::new(), Some(240)),
            ]
        );
        shell
            .send(&ControlToBrain::SegmentPlayed {
                run,
                segment,
                samples: 240,
            })
            .await;
        assert_eq!(shell.state().await, (run, Phase::Done));
        assert_eq!(shell.data().await, DataToShell::RunEnd { run });
        drop(shell);
    };
    let (ended, ()) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
}

/// The answer's first sentence is stored with its step, but the write of the rest and the
/// outcome fails, so the run ends failed(store) holding only that prefix: no point heard over
/// the whole answer is stored against it, though the shell played all of it.
#[tokio::test]
async fn a_spoken_answer_whose_tail_was_lost_stores_no_heard_point() {
    let scratch = Scratch::new("spoken-lost-tail");
    Store::open(&scratch.db())
        .expect("store")
        .conn_for_tests()
        .execute_batch(
            "CREATE TRIGGER lose_tail BEFORE UPDATE OF state ON runs WHEN NEW.state = 'done'
             BEGIN SELECT RAISE(ABORT, 'kept'); END;",
        )
        .expect("trigger");
    let mut brain = spoken(&scratch, "0 text Hello there.\n0 step a\n0 text And more.");
    let ((control, data), mut shell) = duplex();
    let mut tap = Tap::from_env();
    let client = async {
        shell.submit("go").await;
        let (run, _) = shell.state().await;
        let mut played = 0;
        loop {
            match shell.data().await {
                DataToShell::SpeechPcm(SpeechPcm {
                    segment,
                    last: Some(samples),
                    ..
                }) => {
                    played += 1;
                    shell
                        .send(&ControlToBrain::SegmentPlayed {
                            run,
                            segment,
                            samples,
                        })
                        .await;
                }
                DataToShell::RunEnd { .. } => break,
                _ => {}
            }
        }
        assert_eq!(
            played, 1,
            "the whole answer, prefix and tail, played as one segment"
        );
        drop(shell);
        run
    };
    let (ended, run) = tokio::join!(brain.serve(control, data, &mut tap), client);
    assert_eq!(ended, Ended::Lost);
    brain.close().await;
    let conn = rusqlite::Connection::open(scratch.db()).expect("raw open");
    let id = i64::try_from(run.0).expect("small id");
    let texts: Vec<String> = conn
        .prepare("SELECT text FROM events WHERE run = ?1 AND kind = 'assistant_text' ORDER BY id")
        .expect("query")
        .query_map([id], |row| row.get(0))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("texts");
    assert_eq!(
        texts,
        ["Hello there."],
        "the prefix is stored and the tail lost"
    );
    let row: (String, Option<i64>, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT state, heard_segment, heard_sample, heard_text FROM runs WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("the run");
    assert_eq!(row, ("failed".into(), None, None, None));
}

/// A heard point the store thread can't take is dropped: unlike a run's outcome, it answers
/// nothing, so it can't fail the run late.
#[tokio::test]
async fn a_heard_point_the_gone_store_cant_take_answers_nothing() {
    let scratch = Scratch::new("heard-store-gone");
    let mut brain = test_support::brain(&scratch, "0 text a");
    brain
        .store
        .take()
        .expect("store thread")
        .close()
        .await
        .expect("close");
    let run = RunId(1);
    let heard = Heard {
        segment: SegmentId(0),
        sample: 240,
        text: 12,
    };
    let end = StoreOp::EndRun {
        thread: ThreadId(1),
        run,
        text: None,
        revealed: None,
        clear: None,
        end: RunEnd::Done,
    };
    brain
        .apply(vec![Effect::Store(StoreOp::Heard { run, heard })], None)
        .expect("applied");
    assert!(
        brain.inputs.try_recv().is_err(),
        "a lost point answers nothing"
    );
    brain
        .apply(vec![Effect::Store(end)], None)
        .expect("applied");
    assert_eq!(
        brain.inputs.try_recv().ok(),
        Some(Input::StoreFailed { run }),
        "a lost outcome still answers"
    );
    brain.close().await;
}
