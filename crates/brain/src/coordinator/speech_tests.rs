//! A spoken run through the coordinator: draining to `Done`, the stall timer, revokes and the
//! quiet barrier.

use kumochi_protocol::message::{
    ControlToShell, Cursor, DataToShell, Narration, OutputGeneration, Phase, RunId, SegmentId,
    SpeechDecision, SpeechDispatch, SpeechPcm, SpeechRetirement, StepRef,
};

use super::{
    Coordinator, Effect, Input, R, Stale, StoreOp, T, admit, end_run, engine, fresh, run_end,
    spoken, state, text,
};
use crate::coordinator::{DRAIN_STALL, QUIET_TIMEOUT, Synthesized};
use crate::engines::{Finish, TurnEvent};
use crate::store::{Interruption, RunEnd};
use crate::voice::reply::ReportMode;

const RATE: u32 = 24_000;
/// 20 s at 24 kHz.
const WINDOW: u64 = 480_000;
const SEG: SegmentId = SegmentId(0);

#[test]
fn queued_registration_reports_only_an_installed_tuple() {
    let mut core = fresh();
    let output = OutputGeneration(7);
    assert_eq!(
        core.register_queued_output(32_000, output, Narration::Plain),
        Ok(vec![])
    );
    assert_eq!(core.output, Some((32_000, output, ReportMode::Queued)));
    core.handle(Input::AudioOutputUnavailable { generation: output })
        .unwrap();
    assert_eq!(
        core.register_queued_output(RATE, output, Narration::Plain),
        Err(Stale)
    );
    assert_eq!(core.output, None);
}

fn say(core: &mut Coordinator, delta: &str) -> Vec<Effect> {
    core.handle(engine(
        super::engine_key(core, R),
        TurnEvent::TextDelta(delta.to_owned()),
    ))
    .expect("current")
}

fn finish(core: &mut Coordinator) -> Vec<Effect> {
    core.handle(engine(
        super::engine_key(core, R),
        TurnEvent::Finished(Finish::Completed),
    ))
    .expect("current")
}

fn tts(made: Synthesized) -> Input {
    Input::Tts {
        run: R,
        segment: SEG,
        made,
    }
}

fn pcm(seq: u32, samples: usize, last: Option<u32>) -> Effect {
    Effect::Data(DataToShell::SpeechPcm(SpeechPcm {
        run: R,
        segment: SEG,
        output_generation: OutputGeneration(1),
        seq,
        samples: vec![3; samples],
        last,
    }))
}

fn arm(epoch: u32) -> Effect {
    Effect::ArmDrain {
        run: R,
        epoch,
        after: DRAIN_STALL,
    }
}

fn quiet_wait() -> Effect {
    Effect::ArmQuiet {
        run: R,
        after: QUIET_TIMEOUT,
    }
}

fn stopped(cursor: Option<Cursor>) -> Input {
    Input::SpeechStopped { run: R, cursor }
}

fn heard(sample: u32, text: usize) -> Effect {
    Effect::Store(StoreOp::Heard {
        run: R,
        heard: crate::voice::reply::Heard {
            segment: SEG,
            sample,
            text,
        },
    })
}

fn dispatch(segment: u32) -> SpeechDispatch {
    SpeechDispatch {
        run: R,
        output: OutputGeneration(1),
        segment: SegmentId(segment),
    }
}

pub(super) fn queued() -> Coordinator {
    let mut core = fresh();
    core.register_queued_output(RATE, OutputGeneration(1), Narration::Conducted)
        .unwrap();
    core.handle(Input::SpokenReplies { loaded: true }).unwrap();
    core.handle(Input::Turn {
        turn: 1,
        thread: None,
        text: "hi".into(),
    })
    .unwrap();
    admit(&mut core, R);
    core
}

fn queue_drawing(core: &mut Coordinator, index: u32) {
    core.current(R).unwrap().reply.as_mut().unwrap().say(
        StepRef {
            scene: super::S,
            index,
        },
        "line",
        &mut Vec::new(),
    );
}

#[test]
fn closed_queued_reply_ignores_old_drain_until_its_speech_decision() {
    let mut core = queued();
    say(&mut core, "Plain. ");
    queue_drawing(&mut core, 1);
    assert_eq!(finish(&mut core), [arm(1)]);
    let after_plain = core.handle(tts(Synthesized::Done)).unwrap();
    assert!(after_plain.contains(&pcm(0, 0, Some(0))));
    assert!(
        after_plain
            .iter()
            .all(|effect| !matches!(effect, Effect::Speak { .. }))
    );
    assert_eq!(
        core.handle(Input::DrainTimeout { run: R, epoch: 1 }),
        Err(Stale)
    );
    assert_eq!(
        core.handle(Input::DrainTimeout { run: R, epoch: 2 }),
        Err(Stale)
    );
    let key = dispatch(1);
    let allowed = core
        .handle(Input::SpeechDecision(key, SpeechDecision::Allow))
        .unwrap();
    assert_eq!(
        allowed
            .iter()
            .filter(|effect| matches!(
                effect,
                Effect::Speak {
                    segment: SegmentId(1),
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(
        allowed
            .iter()
            .any(|effect| matches!(effect, Effect::ArmDrain { .. }))
    );
    assert_eq!(
        core.handle(Input::SpeechDecision(key, SpeechDecision::Allow)),
        Ok(vec![Effect::Control(ControlToShell::SpeechDecisionTaken(
            key
        ))])
    );
}

#[test]
fn discarded_closed_head_enters_the_next_wait_then_completes() {
    let mut core = queued();
    queue_drawing(&mut core, 0);
    queue_drawing(&mut core, 1);
    assert_eq!(finish(&mut core), []);
    let discard = |segment| Input::SpeechDecision(dispatch(segment), SpeechDecision::Discard);
    assert_eq!(
        core.handle(discard(0)).unwrap(),
        [
            Effect::Control(ControlToShell::SpeechDecisionTaken(dispatch(0))),
            pcm(0, 0, Some(0))
        ]
    );
    let last = core.handle(discard(1)).unwrap();
    assert!(
        last.iter()
            .any(|effect| matches!(effect, Effect::ArmDrain { .. }))
    );
    for segment in [0, 1] {
        let effects = core
            .handle(Input::SpeechRetired(SpeechRetirement {
                run: R,
                output: OutputGeneration(1),
                segment: SegmentId(segment),
                retired: 0,
                heard: 0,
                final_segment: true,
            }))
            .unwrap();
        assert_eq!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { .. }))),
            segment == 1
        );
    }
    assert!(
        core.handle(Input::RunStored { run: R })
            .unwrap()
            .contains(&run_end(R))
    );
}

#[test]
fn stop_replacement_and_session_loss_invalidate_closed_eligibility() {
    for end in 0..3 {
        let (replaced, lost) = (end == 1, end == 2);
        let mut core = queued();
        queue_drawing(&mut core, 0);
        assert_eq!(finish(&mut core), []);
        let ending = match end {
            0 => core.handle(Input::Stop {
                run: R,
                revealed: Vec::new(),
            }),
            1 => core.register_queued_output(RATE, OutputGeneration(2), Narration::Conducted),
            _ => core.handle(Input::ShellLost),
        }
        .unwrap();
        assert!(
            ending
                .iter()
                .any(|effect| matches!(effect, Effect::RevokeSpeech { run: R }))
        );
        assert_eq!(
            core.handle(Input::DrainTimeout { run: R, epoch: 1 }),
            Err(Stale)
        );
        let effects = core.handle(Input::SpeechDecision(dispatch(0), SpeechDecision::Allow));
        if let Ok(effects) = effects {
            assert!(
                effects.iter().all(|effect| !matches!(
                    effect,
                    Effect::Speak { .. } | Effect::ArmDrain { .. }
                ))
            );
        }
        if !lost {
            let quiet = core.handle(stopped(None)).unwrap();
            assert_eq!(
                quiet
                    .iter()
                    .any(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { .. }))),
                replaced
            );
        }
        let stored = core.handle(Input::RunStored { run: R }).unwrap();
        assert_eq!(stored.contains(&run_end(R)), !lost);
        assert_eq!(core.ending_runs(), 0);
    }
}

/// Forty samples into the only segment.
const CUT: Option<Cursor> = Some(Cursor {
    segment: SEG,
    sample: 40,
});

/// A draining run the shell stopped, whose `EndRun` and its `failed(store)` retry are both
/// asked for.
fn stopped_then_failed() -> Coordinator {
    let mut core = draining();
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("current");
    core.handle(Input::StoreFailed { run: R }).expect("retried");
    core
}

/// "Hello there. " spoken, its 100 samples sent and its final chunk out, the producer finished.
fn draining() -> Coordinator {
    let mut core = spoken();
    say(&mut core, "Hello there. ");
    finish(&mut core);
    core.handle(tts(Synthesized::Audio(vec![3; 100])))
        .expect("current");
    core.handle(tts(Synthesized::Done)).expect("current");
    core
}

#[test]
fn scene_failure_keeps_queued_history_open_through_the_worker_final() {
    for (reported_early, receipt_first) in
        [(true, false), (true, true), (false, false), (false, true)]
    {
        let mut core = queued();
        let scene = super::S;
        let mut effects = Vec::new();
        core.current(R).unwrap().reply.as_mut().unwrap().say(
            StepRef { scene, index: 0 },
            "One.",
            &mut Vec::new(),
        );
        core.handle(Input::SpeechDecision(dispatch(0), SpeechDecision::Allow))
            .unwrap();
        for _ in 0..5 {
            core.handle(tts(Synthesized::Audio(vec![3; 2_400])))
                .unwrap();
        }
        let partial = SpeechRetirement {
            run: R,
            output: OutputGeneration(1),
            segment: SEG,
            retired: 6_000,
            heard: 6_000,
            final_segment: false,
        };
        if reported_early {
            core.handle(Input::SpeechRetired(partial)).unwrap();
        }
        core.step_failed(R, scene, &mut effects);
        assert!(effects.contains(&pcm(5, 0, Some(12_000))));
        let final_receipt = Input::SpeechRetired(SpeechRetirement {
            retired: 12_000,
            heard: 12_000,
            final_segment: true,
            ..partial
        });
        if receipt_first {
            core.handle(final_receipt).unwrap();
            assert!(
                finish(&mut core)
                    .iter()
                    .any(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { .. })))
            );
        } else {
            finish(&mut core);
            assert_eq!(core.handle(Input::RunStored { run: R }), Err(Stale));
            assert!(
                core.handle(final_receipt)
                    .unwrap()
                    .iter()
                    .any(|effect| matches!(effect, Effect::Store(StoreOp::EndRun { .. })))
            );
        }
        let saved = core.handle(Input::RunStored { run: R }).unwrap();
        assert!(saved.contains(&heard(12_000, 0)), "{saved:?}");
    }
}

#[test]
fn a_spoken_answer_ends_done_only_after_its_last_segment_played() {
    let mut core = spoken();
    assert_eq!(
        say(&mut core, "Hello there. "),
        vec![
            state(R, T, Phase::Answering),
            text("Hello there. "),
            Effect::Data(DataToShell::SegmentQueued {
                run: R,
                segment: SEG,
                step: None,
                start: kumochi_protocol::message::SegmentStart::Plain,
                output_generation: kumochi_protocol::message::OutputGeneration(1),
            }),
            Effect::Speak {
                run: R,
                segment: SEG,
                rate: RATE,
                text: "Hello there.".into(),
            },
            Effect::Credit {
                run: R,
                samples: WINDOW,
            },
        ]
    );
    assert_eq!(finish(&mut core), vec![arm(1)], "draining, not done");
    assert_eq!(
        core.handle(tts(Synthesized::Audio(vec![3; 100]))),
        Ok(vec![pcm(0, 100, None), arm(2)])
    );
    assert_eq!(
        core.handle(tts(Synthesized::Done)),
        Ok(vec![pcm(1, 0, Some(100)), arm(3)]),
        "the final chunk alone doesn't end the run"
    );
    assert_eq!(
        core.handle(Input::SpeechProgress {
            run: R,
            segment: SEG,
            sample: 60,
        }),
        Ok(vec![arm(4)])
    );
    assert_eq!(
        core.handle(Input::SegmentPlayed {
            run: R,
            segment: SEG,
            samples: 100,
        }),
        Ok(vec![end_run(Some("Hello there. "), None, RunEnd::Done)])
    );
    assert_eq!(
        core.handle(Input::RunStored { run: R }),
        Ok(vec![heard(100, 12), state(R, T, Phase::Done), run_end(R)]),
        "all of the spoken \"Hello there.\" was heard"
    );
}

#[test]
fn the_heard_point_is_stored_before_the_job_its_run_starts() {
    let mut core = draining();
    assert_eq!(
        core.handle(Input::SignedIn { generation: 1 }),
        Ok(Vec::new())
    );
    core.handle(Input::SegmentPlayed {
        run: R,
        segment: SEG,
        samples: 100,
    })
    .expect("current");
    let effects = core.handle(Input::RunStored { run: R }).expect("ending");
    assert_eq!(effects.first(), Some(&heard(100, 12)));
    assert!(
        matches!(effects.get(1), Some(Effect::Store(StoreOp::ReadJob(_)))),
        "the title job's read follows the point: {effects:?}"
    );
}

#[test]
fn a_spoken_run_with_nothing_to_say_ends_at_once() {
    let mut core = spoken();
    assert_eq!(finish(&mut core), vec![end_run(None, None, RunEnd::Done)]);
}

#[test]
fn without_spoken_replies_or_an_output_device_a_run_stays_text() {
    for (loaded, output) in [(false, true), (true, false)] {
        let mut core = fresh();
        core.handle(Input::SpokenReplies { loaded })
            .expect("never stale");
        if output {
            core.handle(Input::AudioOutput {
                rate: RATE,
                generation: OutputGeneration(1),
            })
            .expect("never stale");
        }
        core.handle(Input::Turn {
            turn: 1,
            thread: None,
            text: "hi".into(),
        })
        .expect("a turn is never stale");
        admit(&mut core, R);
        assert_eq!(
            say(&mut core, "Hello there. "),
            vec![state(R, T, Phase::Answering), text("Hello there. ")]
        );
        assert_eq!(
            finish(&mut core),
            vec![end_run(Some("Hello there. "), None, RunEnd::Done)]
        );
    }
}

#[test]
fn a_drain_stall_revokes_the_speech_and_tells_the_end_once_stored_and_quiet() {
    let mut core = draining();
    assert_eq!(
        core.handle(Input::DrainTimeout { run: R, epoch: 2 }),
        Err(Stale),
        "only the latest timer counts"
    );
    let stopped_run = RunEnd::Interrupted(Interruption::Stop);
    assert_eq!(
        core.handle(Input::DrainTimeout { run: R, epoch: 3 }),
        Ok(vec![
            Effect::RevokeSpeech { run: R },
            Effect::Control(ControlToShell::SpeechRevoked { run: R }),
            quiet_wait(),
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            end_run(Some("Hello there. "), None, stopped_run),
        ])
    );
    assert_eq!(
        core.handle(tts(Synthesized::Audio(vec![3; 50]))),
        Ok(Vec::new()),
        "no audio goes out after the revoke"
    );
    assert_eq!(core.handle(Input::RunStored { run: R }), Ok(Vec::new()));
    assert_eq!(
        core.handle(stopped(CUT)),
        Ok(vec![
            heard(40, 0),
            state(R, T, Phase::Interrupted),
            run_end(R)
        ]),
        "cut mid-segment, so the heard text ends where that segment starts"
    );
    assert_eq!(core.ending_runs(), 0);
    assert_eq!(core.handle(Input::QuietTimeout { run: R }), Err(Stale));
}

#[test]
fn a_shell_stop_while_draining_is_told_once_quiet_and_stored_in_either_order() {
    for quiet_first in [true, false] {
        let mut core = draining();
        assert_eq!(
            core.handle(Input::Stop {
                run: R,
                revealed: Vec::new(),
            }),
            Ok(vec![
                Effect::RevokeSpeech { run: R },
                quiet_wait(),
                Effect::CancelEngine { run: R },
                Effect::Revoke { run: R },
                end_run(
                    Some("Hello there. "),
                    Some(Vec::new()),
                    RunEnd::Interrupted(Interruption::Stop)
                ),
            ]),
            "the shell is already stopping, so it isn't told to"
        );
        let told = vec![state(R, T, Phase::Interrupted), run_end(R)];
        if quiet_first {
            assert_eq!(core.handle(stopped(None)), Ok(Vec::new()));
            assert_eq!(core.handle(Input::RunStored { run: R }), Ok(told));
        } else {
            assert_eq!(core.handle(Input::RunStored { run: R }), Ok(Vec::new()));
            assert_eq!(core.handle(stopped(None)), Ok(told));
        }
        assert_eq!(core.ending_runs(), 0);
    }
}

#[test]
fn a_scene_stop_while_draining_is_a_shell_stop_too() {
    let mut core = draining();
    let cuts = kumochi_protocol::message::SceneCuts::new(Vec::new()).expect("no cuts");
    assert_eq!(
        core.handle(Input::StopScenes { run: R, cuts }),
        Ok(vec![
            Effect::RevokeSpeech { run: R },
            quiet_wait(),
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            end_run(
                Some("Hello there. "),
                None,
                RunEnd::Interrupted(Interruption::Stop)
            ),
        ]),
        "the shell is already stopping, so it isn't told to"
    );
}

#[test]
fn a_shell_that_never_goes_quiet_loses_its_connection() {
    let mut core = draining();
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("current");
    assert_eq!(
        core.handle(Input::QuietTimeout { run: R }),
        Ok(vec![Effect::Broken("speech never went quiet")])
    );
    core.handle(stopped(None))
        .expect("the late answer still counts");
    assert_eq!(core.handle(Input::QuietTimeout { run: R }), Err(Stale));
}

#[test]
fn a_second_quiet_answer_changes_nothing_and_an_impossible_report_breaks_the_shell() {
    let mut core = draining();
    assert_eq!(
        core.handle(stopped(None)),
        Err(Stale),
        "nothing was revoked"
    );
    assert_eq!(
        core.handle(Input::SegmentPlayed {
            run: R,
            segment: SEG,
            samples: 101,
        }),
        Ok(vec![Effect::Broken("a speech report that can't be true")])
    );
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("current");
    core.handle(stopped(None)).expect("the first answer");
    assert_eq!(
        core.handle(stopped(None)),
        Err(Stale),
        "a Stop that crossed a revoke is answered twice"
    );
}

#[test]
fn a_device_change_stops_the_speech_and_the_run_ends_done_once_the_shell_is_quiet() {
    let mut core = spoken();
    say(&mut core, "One. ");
    assert_eq!(
        core.handle(Input::AudioOutput {
            rate: 48_000,
            generation: OutputGeneration(2),
        }),
        Ok(vec![
            Effect::RevokeSpeech { run: R },
            Effect::Control(ControlToShell::SpeechRevoked { run: R }),
            quiet_wait(),
        ])
    );
    assert_eq!(
        say(&mut core, "Two. "),
        vec![text("Two. ")],
        "a revoked reply speaks and credits nothing more"
    );
    assert_eq!(finish(&mut core), Vec::new(), "it waits for the shell");
    assert_eq!(
        core.handle(stopped(None)),
        Ok(vec![end_run(Some("One. Two. "), None, RunEnd::Done)])
    );
    assert_eq!(
        core.handle(Input::RunStored { run: R }),
        Ok(vec![state(R, T, Phase::Done), run_end(R)])
    );
}

#[test]
fn withdrawing_the_output_stops_its_speech_and_leaves_later_runs_without_a_device() {
    let mut core = spoken();
    say(&mut core, "One. ");
    let withdrawn = Input::AudioOutputUnavailable {
        generation: OutputGeneration(1),
    };
    assert_eq!(
        core.handle(withdrawn),
        Ok(vec![
            Effect::RevokeSpeech { run: R },
            Effect::Control(ControlToShell::SpeechRevoked { run: R }),
            quiet_wait(),
        ])
    );
    assert_eq!(core.output, None, "no later run makes a reply");
    assert_eq!(
        core.handle(Input::AudioOutputUnavailable {
            generation: OutputGeneration(1),
        }),
        Ok(Vec::new()),
        "a repeat is a no-op"
    );
    assert_eq!(finish(&mut core), Vec::new(), "it waits for the shell");
    assert_eq!(
        core.handle(stopped(None)),
        Ok(vec![end_run(Some("One. "), None, RunEnd::Done)])
    );
}

#[test]
fn only_a_strictly_newer_generation_announces_after_a_withdrawal() {
    let mut core = super::fresh();
    let withdrawn = Input::AudioOutputUnavailable {
        generation: OutputGeneration(2),
    };
    let announce = |generation| Input::AudioOutput {
        rate: RATE,
        generation: OutputGeneration(generation),
    };
    assert_eq!(core.handle(announce(2)), Ok(Vec::new()));
    assert_eq!(core.handle(withdrawn), Ok(Vec::new()));
    for stale in [2, 1] {
        assert_eq!(core.handle(announce(stale)), Ok(Vec::new()));
        assert_eq!(core.output, None, "generation {stale} stays withdrawn");
    }
    assert_eq!(core.handle(announce(3)), Ok(Vec::new()));
    assert_eq!(
        core.output,
        Some((RATE, OutputGeneration(3), ReportMode::Legacy))
    );
}

#[test]
fn withdrawing_another_generation_changes_nothing() {
    let mut core = spoken();
    say(&mut core, "One. ");
    assert_eq!(
        core.handle(Input::AudioOutputUnavailable {
            generation: OutputGeneration(2),
        }),
        Ok(Vec::new())
    );
    assert_eq!(
        core.output,
        Some((RATE, OutputGeneration(1), ReportMode::Legacy))
    );
}

#[test]
fn a_new_turn_while_draining_revokes_the_old_speech_and_starts_at_once() {
    let mut core = draining();
    let effects = core
        .handle(Input::Turn {
            turn: 2,
            thread: Some(T),
            text: "next".into(),
        })
        .expect("a turn is never stale");
    let next = RunId(11);
    assert_eq!(
        effects,
        vec![
            Effect::RevokeSpeech { run: R },
            Effect::Control(ControlToShell::SpeechRevoked { run: R }),
            quiet_wait(),
            Effect::CancelEngine { run: R },
            Effect::Revoke { run: R },
            end_run(
                Some("Hello there. "),
                None,
                RunEnd::Interrupted(Interruption::Replaced)
            ),
            Effect::Store(StoreOp::BeginTurn {
                thread: T,
                title: None,
                run: next,
                text: "next".into(),
                screen: None,
            }),
            Effect::Control(ControlToShell::RunState {
                run: next,
                thread: T,
                phase: Phase::Thinking,
                error: None,
                origin: Some(kumochi_protocol::message::RunOrigin::Typed { turn: 2 }),
            }),
            Effect::ArmDeadline {
                run: next,
                after: crate::coordinator::WALL_CLOCK,
            },
        ]
    );
}

#[test]
fn a_shell_lost_while_waiting_for_quiet_lets_the_run_go_untold() {
    let mut core = draining();
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("current");
    assert_eq!(core.handle(Input::RunStored { run: R }), Ok(Vec::new()));
    core.handle(Input::SpeechProgress {
        run: R,
        segment: SEG,
        sample: 40,
    })
    .expect("current");
    assert_eq!(
        core.handle(Input::ShellLost),
        Ok(vec![heard(40, 0)]),
        "the point played before the loss is kept, though nobody is told"
    );
    assert_eq!(core.ending_runs(), 0);
}

#[test]
fn a_store_failure_while_quieting_is_told_once_as_failed() {
    let mut core = draining();
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("current");
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }),
        Ok(vec![Effect::Store(StoreOp::EndRun {
            screen: None,
            thread: T,
            run: R,
            text: None,
            revealed: Some(Vec::new()),
            clear: None,
            end: RunEnd::Failed(crate::store::Failure::Store),
        })])
    );
    core.handle(stopped(None)).expect("quiet");
    assert_eq!(
        core.handle(Input::RunStored { run: R }),
        Ok(vec![state(R, T, Phase::Failed), run_end(R)])
    );
}

#[test]
fn a_run_whose_store_write_failed_stores_no_heard_point() {
    let mut core = stopped_then_failed();
    assert_eq!(core.handle(stopped(CUT)), Ok(Vec::new()));
    assert_eq!(
        core.handle(Input::RunStored { run: R }),
        Ok(vec![state(R, T, Phase::Failed), run_end(R)]),
        "the committed row may lack the heard text"
    );
}

#[test]
fn an_abandoned_run_stores_no_heard_point_and_ignores_late_reports() {
    let mut core = stopped_then_failed();
    assert_eq!(
        core.handle(Input::StoreFailed { run: R }),
        Ok(vec![state(R, T, Phase::Failed), run_end(R)]),
        "the retry failed too, so the run is given up and told at once"
    );
    assert_eq!(core.ending_runs(), 0);
    assert_eq!(core.handle(stopped(CUT)), Err(Stale));
    assert_eq!(core.handle(Input::QuietTimeout { run: R }), Err(Stale));
}

#[test]
fn a_deleted_runs_late_reports_store_no_heard_point() {
    for (failed, stored, lost) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (false, false, true),
    ] {
        let mut core = draining();
        core.handle(Input::Stop {
            run: R,
            revealed: Vec::new(),
        })
        .expect("current");
        if failed {
            core.handle(Input::StoreFailed { run: R })
                .expect("quieting outcome failed");
        }
        if stored {
            core.handle(Input::RunStored { run: R })
                .expect("stored while still quieting");
        }
        if lost {
            core.handle(Input::ShellLost).expect("lost while quieting");
        }
        assert_eq!(core.ending_runs(), 1);
        core.handle(Input::Delete {
            thread: T,
            after_turns: 1,
            resend: false,
        })
        .expect("asked");
        let mut expected = vec![Effect::Control(ControlToShell::ThreadDeleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::Removed,
            last_run: Some(R),
        })];
        if !lost {
            expected.push(run_end(R));
        }
        assert_eq!(
            core.handle(Input::Deleted {
                thread: T,
                outcome: kumochi_protocol::message::Deleted::Removed,
                last_run: None,
            }),
            Ok(expected)
        );
        assert_eq!(core.handle(stopped(CUT)), Err(Stale));
        assert_eq!(core.handle(Input::RunStored { run: R }), Err(Stale));
        assert_eq!(core.handle(Input::StoreFailed { run: R }), Err(Stale));
        assert_eq!(core.ending_runs(), 0);
    }
}

#[test]
fn speech_effects_and_inputs_show_no_text_or_audio() {
    let marker = "SENTINEL-7b2";
    let speak = Effect::Speak {
        run: R,
        segment: SEG,
        rate: RATE,
        text: marker.into(),
    };
    let audio = tts(Synthesized::Audio(vec![12_345; 3]));
    for shown in [format!("{speak:?}"), format!("{audio:?}")] {
        assert!(!shown.contains(marker), "{shown}");
        assert!(!shown.contains("12345"), "{shown}");
        assert!(
            !shown
                .as_bytes()
                .windows(marker.len())
                .any(|window| window == marker.as_bytes()),
            "{shown}"
        );
    }
}
