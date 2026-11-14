//! A spoken run through the coordinator: draining to `Done`, the stall timer, revokes and the
//! quiet barrier.

use kumochi_protocol::message::{
    ControlToShell, Cursor, DataToShell, OutputGeneration, Phase, RunId, SegmentId, SpeechPcm,
};

use super::{
    Coordinator, Effect, Input, R, Stale, StoreOp, T, admit, end_run, engine, fresh, run_end,
    spoken, state, text,
};
use crate::coordinator::{DRAIN_STALL, QUIET_TIMEOUT, Synthesized};
use crate::engines::{Finish, TurnEvent};
use crate::store::{Interruption, RunEnd};

const RATE: u32 = 24_000;
/// 20 s at 24 kHz.
const WINDOW: u64 = 480_000;
const SEG: SegmentId = SegmentId(0);

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
    let mut core = draining();
    core.handle(Input::Stop {
        run: R,
        revealed: Vec::new(),
    })
    .expect("current");
    core.handle(Input::Delete {
        thread: T,
        after_turns: 1,
        resend: false,
    })
    .expect("asked");
    assert_eq!(
        core.handle(Input::Deleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::Removed,
            last_run: None,
        }),
        Ok(vec![Effect::Control(ControlToShell::ThreadDeleted {
            thread: T,
            outcome: kumochi_protocol::message::Deleted::Removed,
            last_run: Some(R),
        })])
    );
    assert_eq!(core.handle(stopped(CUT)), Err(Stale));
    assert_eq!(core.handle(Input::RunStored { run: R }), Err(Stale));
    assert_eq!(core.ending_runs(), 0);
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
