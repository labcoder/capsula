//! A spoken run through the core loop: TTS audio reaches the shell through the pump, and the run
//! ends `Done` once the shell played it.

use std::path::Path;

use kumochi_protocol::message::{
    ControlToBrain, DataToShell, OutputGeneration, Phase, SegmentId, SpeechPcm,
};

use super::{Ended, duplex};
use crate::coordinator::Input;
use crate::tap::Tap;
use crate::test_support::{self, Scratch};
use crate::voice::pump::Pump;
use crate::voice::tts::{Engine, TtsError, TtsHandle};

/// Speaks every segment as one frame of 240 silent samples.
struct Silence;

impl Engine for Silence {
    type Stream = std::iter::Once<Result<Vec<f32>, TtsError>>;
    fn stream(&self, _text: &str) -> Result<Self::Stream, TtsError> {
        Ok(std::iter::once(Ok(vec![0.0; 240])))
    }
}

#[tokio::test]
async fn a_spoken_answer_streams_through_the_pump_and_ends_done_once_played() {
    let scratch = Scratch::new("spoken");
    let mut brain = test_support::brain(&scratch, "0 text Hello there.");
    let tts = TtsHandle::spawn_with(tokio::runtime::Handle::current(), |_: &Path| Ok(Silence))
        .expect("the owner thread starts");
    tts.load("models".into());
    brain.pump = Some(Pump::new(tts));
    brain.offline(Input::SpokenReplies { loaded: true });
    brain.offline(Input::AudioOutput {
        rate: 24_000,
        generation: OutputGeneration(1),
    });
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
