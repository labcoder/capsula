use kumochi_protocol::message::{OutputGeneration, RunId, SceneId, SegmentId, StepRef};

use super::*;
use crate::speaker::conductor::{Assignment, SourceSpan};

#[test]
fn ended_output_refuses_onset_follow_frozen_and_fenced_payloads() {
    let span = SourceSpan {
        source: 0,
        at: Duration::ZERO,
        submitted_through: 8,
    };
    for refused in [
        Picture::At(Duration::ZERO),
        Picture::Shown(Motion::Follow(span)),
        Picture::Shown(Motion::Frozen),
    ] {
        assert_eq!(picture_state(refused, Completion::Ended), None);
    }
    assert_eq!(
        picture_state(Picture::Fenced, Completion::Ended),
        Some(PictureState::Owned(Picture::Fenced))
    );
    for accepted in [
        Picture::Wait,
        Picture::Silent,
        Picture::Authored,
        Picture::Shown(Motion::Authored),
    ] {
        assert_eq!(
            picture_state(accepted, Completion::Ended),
            Some(PictureState::Ended(accepted))
        );
    }
}

#[test]
fn stale_route_heads_cannot_produce_completion_but_a_logical_empty_final_can() {
    let key = Assignment {
        session: 3,
        run: RunId(4),
        output: OutputGeneration(5),
        segment: SegmentId(0),
        step: StepRef {
            scene: SceneId(2),
            index: 0,
        },
    };
    let mut core = Conductor::new(key, &Start::Speech);
    for (route, head) in [
        (Route::Live(7), Head::WaitingForPcm),
        (Route::Rebuilding, Head::WaitingForPcm),
        (Route::Virtual, Head::WaitingForPcm),
        (Route::Live(7), Head::Played),
        (Route::Live(8), Head::WaitingForPcm),
        (Route::Live(7), Head::Revoked),
    ] {
        core.update(
            key,
            Duration::ZERO,
            View::Preparing,
            Playback {
                route,
                head,
                handoff: None,
                synthesis: None,
            },
        )
        .unwrap();
        assert!(
            core.completion() == Completion::Active,
            "{route:?} {head:?} ended the binding"
        );
    }
    let decision = core
        .update(
            key,
            Duration::ZERO,
            View::Preparing,
            Playback {
                route: Route::Rebuilding,
                head: Head::AdvancingPredecessor,
                handoff: None,
                synthesis: Some(SynthesisFinal {
                    segment: key.segment,
                    samples: 0,
                }),
            },
        )
        .unwrap();
    assert!(core.completion() == Completion::Ended);
    assert_eq!(
        picture_state(decision.picture, core.completion()),
        Some(PictureState::Ended(Picture::Wait))
    );

    let later = core
        .update(
            key,
            Duration::from_millis(1),
            View::Eligible,
            Playback {
                route: Route::Live(9),
                head: Head::WaitingForPcm,
                handoff: None,
                synthesis: None,
            },
        )
        .unwrap();
    assert_eq!(
        picture_state(later.picture, core.completion()),
        Some(PictureState::Ended(Picture::Silent))
    );
}
