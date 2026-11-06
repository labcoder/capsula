//! A spoken run through the core loop: TTS audio reaches the shell through the pump, and the run
//! ends `Done` once the shell played it.

use std::path::Path;

use kumochi_protocol::message::{
    ControlToBrain, DataToShell, OutputGeneration, Phase, RunId, SegmentId, SpeechPcm, ThreadId,
};

use super::{Brain, Ended, duplex};
use crate::coordinator::{Effect, Input, StoreOp};
use crate::store::{RunEnd, Store};
use crate::tap::Tap;
use crate::test_support::{self, Scratch};
use crate::voice::pump::Pump;
use crate::voice::reply::Heard;
use crate::voice::tts::{Engine, TtsError, TtsHandle};

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
    let tts = TtsHandle::spawn_with(tokio::runtime::Handle::current(), |_: &Path| Ok(Silence))
        .expect("the owner thread starts");
    tts.load("models".into());
    brain.pump = Some(Pump::new(tts));
    brain.offline(Input::SpokenReplies { loaded: true });
    brain.offline(Input::AudioOutput {
        rate: 24_000,
        generation: OutputGeneration(1),
    });
    brain
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
