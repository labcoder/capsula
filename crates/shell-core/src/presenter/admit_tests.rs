//! The admission lane: one commit at a time, its deadline, Stop and clear around it, busy
//! consumption and the credit it withholds.

use std::sync::{Arc, atomic::Ordering, mpsc};

use kumochi_protocol::message::{
    ControlToBrain, DATA_WINDOW, DataToShell, RunId, SceneCut, SceneCuts, SceneDescriptorV1,
    SceneFailure, SceneId,
};
use kumochi_scene::{
    SceneEvent,
    budget::Usage,
    deferred::Deferred,
    limits::{MAX_GLOBAL_LIVE_OBJECTS, MAX_RESIDENT_BYTES},
    text::Bounded,
};

use super::super::{Cut, window::ConsumeError};
use super::{
    Broken, Completion, Effect, FRAME, GENERATION, LIVE, Presenter, RUN, Shaper, acknowledge,
    admissions, arrive, begin, clear_drawing, commit, commit_as, commit_frame, controls, credit,
    drained, exhausted, failed, header, hold_dropper, jobs, label, live, ms, only_admission,
    only_job, recovering, rect, stop_scenes, zero_cut,
};

const OTHER: SceneId = SceneId(21);

#[test]
fn owned_stop_classifies_running_ready_installed_and_queued_work_without_stopping_another_scene() {
    for stage in ["running", "ready", "installed", "queued"] {
        let (mut presenter, now) = live();
        begin(&mut presenter, OTHER, now);
        let other = only_job(
            commit_as(&mut presenter, RunId(3), OTHER, 0, rect("other"), now)
                .expect("unowned worker"),
        );
        begin(&mut presenter, LIVE, now);
        let first_run = if stage == "queued" { RunId(4) } else { RUN };
        let first = only_job(
            commit_as(&mut presenter, first_run, LIVE, 0, rect("first"), now)
                .expect("first worker"),
        );
        let mut late = Vec::new();
        if matches!(stage, "running" | "queued") {
            late.push(first);
        } else {
            presenter
                .completed(first.run(&mut Shaper), now)
                .expect("prepared owner");
        }
        if stage == "installed" {
            presenter.advance(GENERATION, now).expect("installed owner");
            late.push(only_job(
                commit_as(&mut presenter, RunId(6), LIVE, 1, rect("suffix"), now)
                    .expect("newer same-scene suffix"),
            ));
        }
        if stage == "queued" {
            let effects =
                commit_as(&mut presenter, RUN, LIVE, 1, rect("queued"), now).expect("queued owner");
            assert!(jobs(effects).is_empty());
            assert_eq!(presenter.scenes[&LIVE].queue.len(), 1);
        }
        let effects = presenter.stop_owned(RUN, now).expect("scoped stop");
        assert_eq!(credit(&effects), 0);
        assert_eq!(
            controls(&effects),
            [&ControlToBrain::StopScenes {
                run: RUN,
                cuts: SceneCuts::new(vec![SceneCut {
                    scene: LIVE,
                    cut: kumochi_scene::StopCut {
                        activated: u32::from(stage == "installed"),
                        revealed: Bounded::default(),
                    }
                }])
                .expect("one owned scene"),
            }],
            "{stage}"
        );
        assert_eq!(presenter.scenes[&OTHER].cut, Cut::Open, "{stage}");
        assert!(presenter.scenes[&LIVE].ready.is_empty());
        assert!(presenter.scenes[&LIVE].queue.is_empty());
        for worker in late {
            assert!(
                presenter
                    .completed(worker.run(&mut Shaper), now)
                    .expect("late cancelled worker")
                    .is_empty(),
                "{stage}"
            );
        }
        assert_eq!(presenter.workers(), 1);
        presenter
            .completed(other.run(&mut Shaper), now)
            .expect("unowned worker survives");
        assert_eq!(presenter.workers(), 0);
        assert_eq!(presenter.scenes[&OTHER].ready.len(), 1);
    }
}

#[test]
fn consume_checks_cut_and_skipping_before_remembering_a_busy_request() {
    for (cut, skipping) in [(Cut::Outstanding, false), (Cut::Open, true)] {
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        let first = only_job(commit(&mut presenter, LIVE, 0, rect("first"), now).expect("first"));
        presenter
            .completed(first.run(&mut Shaper), now)
            .expect("prepared");
        let admission = only_admission(
            arrive(&mut presenter, commit_frame(LIVE, 1, rect("next")), now).expect("lane"),
        );
        let scene = presenter.scenes.get_mut(&LIVE).expect("scene");
        scene.cut = cut;
        scene.skipping = skipping;
        assert_eq!(
            presenter
                .consume_ready(LIVE, 0, now, 0, 0)
                .expect_err("closed consumer"),
            ConsumeError::NotReady
        );
        assert_eq!(presenter.scenes[&LIVE].pending_consumption, None);
        drop(admission);
    }
}

#[test]
fn two_owned_tags_survive_raw_retirement_and_recovery_before_fifo_admission() {
    use crate::runs::{RunDisposition, RunTail, Runs};
    use kumochi_protocol::message::{Phase, ThreadId};
    let mut runs = Runs::default();
    let (mut presenter, now, recovery) = recovering(vec![(LIVE, rect("saved"))]);
    for (run, scene) in [
        (RunId(7), OTHER),
        (RunId(8), SceneId(22)),
        (RunId(9), SceneId(23)),
    ] {
        runs.apply_state(run, ThreadId(run.0), Phase::Thinking, None);
        begin(&mut presenter, scene, now);
        assert!(
            commit_as(&mut presenter, run, scene, 0, rect("received"), now)
                .expect("held commit")
                .is_empty()
        );
        if run != RunId(9) {
            runs.stop_owned(RunTail {
                run,
                thread: ThreadId(run.0),
                control_closed: false,
                data_ended: false,
            });
            presenter.stop_owned(run, now).expect("mark received work");
        }
    }
    for run in [RunId(7), RunId(8)] {
        assert_eq!(runs.apply_data(DataToShell::RunEnd { run }, 4), 4);
        assert!(
            !runs
                .apply_state(run, ThreadId(run.0), Phase::Done, None)
                .visible
        );
        assert_eq!(runs.owned_disposition(run), RunDisposition::Live);
    }
    let mut effects = presenter
        .completed(recovery.run(&mut Shaper), now)
        .expect("preserved recovery");
    assert_eq!(credit(&effects), FRAME);
    for (run, scene) in [(RunId(7), OTHER), (RunId(8), SceneId(22))] {
        effects = presenter
            .completed(only_admission(effects).run(), now)
            .expect("suppressed admitted candidate");
        assert_eq!(credit(&effects), 2 * FRAME);
        assert_eq!(
            controls(&effects),
            [&ControlToBrain::StepFailed {
                run,
                scene,
                index: 0,
                reason: SceneFailure::Cancelled,
                cut: zero_cut()
            }]
        );
        assert_eq!(presenter.scenes[&scene].state.view().revision(), 1);
        assert!(presenter.scenes[&scene].queue.is_empty());
    }
    let effects = presenter
        .completed(only_admission(effects).run(), now)
        .expect("unowned admission");
    assert_eq!(credit(&effects), FRAME);
    assert!(controls(&effects).is_empty());
    let job = only_job(effects);
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("unowned preparation");
    assert_eq!(presenter.stoppable_run(), Some(RunId(9)));
    assert_eq!(presenter.scenes[&SceneId(23)].ready.len(), 1);
    assert!(presenter.held.is_empty());
}

#[test]
fn owned_stop_keeps_semantic_admission_and_unowned_work_but_rejects_consumption() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let old = only_job(
        commit_as(&mut presenter, RunId(6), LIVE, 0, rect("old"), now).expect("old admission"),
    );
    presenter
        .completed(old.run(&mut Shaper), now)
        .expect("old prepared");
    begin(&mut presenter, OTHER, now);
    let first =
        only_job(commit(&mut presenter, OTHER, 0, rect("owned"), now).expect("owned admission"));
    presenter
        .completed(first.run(&mut Shaper), now)
        .expect("owned prepared");
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(OTHER, 1, rect("late")), now).expect("lane"),
    );
    assert_eq!(
        presenter
            .consume_ready(OTHER, 0, now, 0, 0)
            .expect_err("busy"),
        ConsumeError::Busy
    );
    let effects = presenter.stop_owned(RUN, now).expect("owned stop");
    assert_eq!(credit(&effects), 0);
    assert_eq!(
        controls(&effects),
        [&ControlToBrain::StopScenes {
            run: RUN,
            cuts: SceneCuts::new(vec![SceneCut {
                scene: OTHER,
                cut: zero_cut()
            }])
            .expect("one cut")
        }]
    );
    assert_eq!(presenter.scenes[&LIVE].ready.len(), 1);
    assert_eq!(presenter.scenes[&LIVE].cut, Cut::Open);
    assert_eq!(
        presenter
            .consume_ready(OTHER, 0, now, 0, 0)
            .expect_err("stopped"),
        ConsumeError::NotReady
    );
    assert_eq!(presenter.scenes[&OTHER].pending_consumption, None);
    let effects = presenter
        .completed(admission.run(), now)
        .expect("still semantically admitted");
    assert_eq!(credit(&effects), FRAME);
    assert!(controls(&effects).is_empty());
    assert!(jobs(effects).is_empty());
    assert_eq!(presenter.scenes[&OTHER].state.view().revision(), 2);
    assert_eq!(presenter.scenes[&OTHER].state.view().usage().objects, 2);
    assert_eq!(presenter.stoppable_run(), Some(RunId(6)));
    assert_eq!(presenter.scenes[&OTHER].cut, Cut::Outstanding);
}

#[test]
fn stopped_commit_from_open_or_applied_mirrors_then_requests_one_exact_cut() {
    for applied in [false, true] {
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        let first = only_job(commit(&mut presenter, LIVE, 0, rect("first"), now).expect("first"));
        presenter
            .completed(first.run(&mut Shaper), now)
            .expect("prepared");
        presenter.advance(GENERATION, now).expect("activated");
        presenter.sample_at(now).expect("sampled");
        let cut = presenter.scenes[&LIVE].cut_now();
        if applied {
            presenter.stop_owned(RUN, now).expect("stopped");
            acknowledge(&mut presenter, LIVE, 1, cut.clone(), now).expect("cut applied");
        }
        let effects = presenter
            .data_with_disposition(
                GENERATION,
                Deferred::new(commit_frame(LIVE, 1, rect("late")), 0),
                FRAME,
                if applied {
                    crate::runs::RunDisposition::Live
                } else {
                    crate::runs::RunDisposition::Stopped
                },
                now,
            )
            .expect("late admission");
        if applied {
            presenter
                .stop_owned(RUN, now)
                .expect("mark the already-received lane");
        }
        assert_eq!(credit(&effects), 0);
        assert!(presenter.scenes[&LIVE].skipping);
        assert_eq!(presenter.scenes[&LIVE].stopped_ms, Some(0));
        assert_eq!(
            presenter
                .consume_ready(LIVE, 1, now, 0, 0)
                .expect_err("skipping"),
            ConsumeError::NotReady
        );
        let effects = presenter
            .completed(only_admission(effects).run(), now)
            .expect("mirrored");
        assert_eq!(credit(&effects), FRAME);
        assert_eq!(
            controls(&effects),
            [&ControlToBrain::StepFailed {
                run: RUN,
                scene: LIVE,
                index: 1,
                reason: SceneFailure::Cancelled,
                cut: cut.clone()
            }]
        );
        assert!(jobs(effects).is_empty());
        assert_eq!(presenter.scenes[&LIVE].state.view().revision(), 2);
        let effects = presenter
            .data_with_disposition(
                GENERATION,
                Deferred::new(commit_frame(LIVE, 2, rect("more")), 0),
                FRAME,
                crate::runs::RunDisposition::Stopped,
                now,
            )
            .expect("outstanding admission");
        assert_eq!(credit(&effects), 0);
        let effects = presenter
            .completed(only_admission(effects).run(), now)
            .expect("outstanding mirror");
        assert_eq!(credit(&effects), FRAME);
        assert!(controls(&effects).is_empty());
        assert!(jobs(effects).is_empty());
        assert_eq!(presenter.scenes[&LIVE].state.view().revision(), 3);
        assert_eq!(
            acknowledge(&mut presenter, LIVE, 1, cut.clone(), now).err(),
            Some(Broken::Cut)
        );
        acknowledge(&mut presenter, LIVE, 3, cut, now).expect("exact acknowledgement");
        let next = commit_as(&mut presenter, RunId(8), LIVE, 3, rect("unowned"), now)
            .expect("later unowned admission");
        assert_eq!(jobs(next).len(), 1);
    }
}

#[test]
fn stop_drains_a_pending_consumption_without_a_worker_and_keeps_later_commits_unprepared() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    presenter
        .completed(first.run(&mut Shaper), now)
        .expect("prepared");
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 1, rect("b")), now).expect("admission"),
    );
    assert_eq!(
        presenter
            .consume_ready(LIVE, 0, now, 0, 0)
            .expect_err("busy"),
        ConsumeError::Busy
    );
    let prepared = only_job(presenter.completed(admission.run(), now).expect("admitted"));
    presenter
        .completed(prepared.run(&mut Shaper), now)
        .expect("prepared");
    assert_eq!(presenter.workers(), 0);
    assert_eq!(presenter.deadline(), None);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, Some(0));
    assert!(
        arrive(&mut presenter, commit_frame(LIVE, 2, rect("c")), now)
            .expect("held commit")
            .is_empty()
    );
    assert_eq!(presenter.held_bytes, FRAME);

    let mut stopped = presenter.stop(RUN, now).expect("stop");
    assert!(matches!(stopped.last(), Some(Effect::Control(message))
        if *message == stop_scenes(vec![SceneCut { scene: LIVE, cut: zero_cut() }])));
    let position = stopped
        .iter()
        .position(|effect| matches!(effect, Effect::Admit(_)))
        .expect("Stop drains the held commit without waiting for another input");
    let Effect::Admit(admission) = stopped.remove(position) else {
        unreachable!()
    };
    assert!(presenter.held.is_empty());
    assert_eq!(presenter.held_bytes, 0);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, None);
    let effects = presenter
        .completed(admission.run(), now)
        .expect("fenced admission");
    assert_eq!(credit(&effects), FRAME);
    assert!(controls(&effects).is_empty());
    assert!(
        jobs(effects).is_empty(),
        "the drained commit never prepares"
    );
    let effects = commit(&mut presenter, LIVE, 3, rect("d"), now).expect("post-Stop commit");
    assert!(controls(&effects).is_empty());
    assert!(
        jobs(effects).is_empty(),
        "the post-Stop commit never prepares"
    );
    let scene = &presenter.scenes[&LIVE];
    assert_eq!(scene.state.view().revision(), 4);
    assert_eq!(scene.state.view().activated(), 0);
    assert_eq!(scene.view.activated(), 0);
    assert!(scene.ready.is_empty());
    assert_eq!(
        presenter
            .frames
            .borrow()
            .painted(LIVE)
            .map(|paint| (paint.epoch, paint.bound)),
        None
    );
    let effects =
        acknowledge(&mut presenter, LIVE, 4, zero_cut(), now).expect("cut acknowledgement");
    assert!(controls(&effects).is_empty());
    assert!(jobs(effects).is_empty());
    assert!(!presenter.holds_scenes());
    assert!(
        presenter
            .tick(ms(now, 5_000))
            .expect("past all old deadlines")
            .is_empty()
    );
}

#[test]
fn a_fresh_commit_after_the_wake_waits_for_the_pending_consumption_retry() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    assert!(
        presenter
            .completed(first.run(&mut Shaper), ms(now, 1))
            .expect("prepared")
            .is_empty()
    );
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 1, rect("b")), ms(now, 2))
            .expect("the next admission"),
    );
    assert_eq!(
        presenter
            .consume_ready(LIVE, 0, ms(now, 3), 0, 0)
            .expect_err("busy"),
        ConsumeError::Busy
    );
    let effects = presenter
        .completed(admission.run(), ms(now, 4))
        .expect("admitted");
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::ConsumptionReady {
            scene: LIVE,
            index: 0
        }
    )));
    assert!(presenter.lane.is_none());
    assert!(presenter.held.is_empty());
    let usage = presenter.global.usage();
    let effects = arrive(&mut presenter, commit_frame(LIVE, 2, rect("c")), ms(now, 5))
        .expect("a fresh commit before retry");
    assert!(
        effects.is_empty(),
        "the pending retry must precede admission"
    );
    assert!(presenter.lane.is_none());
    assert_eq!(presenter.held.len(), 1);
    assert_eq!(presenter.held_bytes, FRAME);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, Some(0));
    assert_eq!(presenter.scenes[&LIVE].state.view().revision(), 2);
    assert_eq!(presenter.global.usage(), usage);
    let consumed = presenter
        .consume_ready(LIVE, 0, ms(now, 6), 0, 0)
        .expect("retry");
    assert_eq!(consumed.step.step().index, 0);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, None);
    drop(only_admission(
        consumed.effects.expect("the held admission"),
    ));
    assert!(presenter.held.is_empty());
    assert_eq!(presenter.held_bytes, 0);
}

#[test]
fn a_consumption_during_admission_is_busy_then_served_before_the_scene_admits_again() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    let answered = presenter
        .completed(first.run(&mut Shaper), ms(now, 1))
        .expect("an answer");
    assert!(answered.is_empty());
    let frame = commit_frame(LIVE, 1, rect("b"));
    let admission = only_admission(arrive(&mut presenter, frame, now).expect("a step"));
    let frame = commit_frame(LIVE, 2, rect("c"));
    assert!(
        arrive(&mut presenter, frame, now)
            .expect("a held step")
            .is_empty()
    );
    let shape = |presenter: &Presenter| {
        let scene = &presenter.scenes[&LIVE];
        (
            scene.ready.len(),
            scene.retained,
            scene.state.view().activated(),
            presenter.residency(),
            presenter.global.usage(),
        )
    };
    let before = shape(&presenter);
    let refused = presenter
        .consume_ready(LIVE, 1, now, 0, 0)
        .expect_err("not the head");
    assert_eq!(refused, ConsumeError::OutOfOrder);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, None);
    let refused = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect_err("busy");
    assert_eq!(refused, ConsumeError::Busy);
    assert_eq!(shape(&presenter), before);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, Some(0));
    let effects = presenter
        .completed(admission.run(), ms(now, 5))
        .expect("the admission");
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::ConsumptionReady {
                scene: LIVE,
                index: 0
            }
        )),
        "{effects:?}"
    );
    assert!(admissions(effects).is_empty(), "the next commit waits");
    assert_eq!(presenter.held.len(), 1);
    let done = presenter
        .consume_ready(LIVE, 0, ms(now, 6), 0, 0)
        .expect("the woken retry");
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, None);
    assert_eq!(presenter.scenes[&LIVE].state.view().activated(), 1);
    assert_eq!(done.step.step().index, 0);
    drop(only_admission(done.effects.expect("a current scene")));
    assert!(presenter.held.is_empty());
}

#[test]
fn a_late_admission_clears_only_its_scene_and_releases_exactly_what_was_mirrored() {
    for tick_first in [true, false] {
        let (mut presenter, now) = live();
        begin(&mut presenter, OTHER, now);
        let alone = (presenter.global.usage(), presenter.residency());
        begin(&mut presenter, LIVE, now);
        let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
        let answered = presenter
            .completed(first.run(&mut Shaper), ms(now, 1))
            .expect("an answer");
        assert!(answered.is_empty());
        let frame = commit_frame(LIVE, 1, rect("b"));
        let admission = only_admission(arrive(&mut presenter, frame, now).expect("a step"));
        let frame = commit_frame(OTHER, 0, rect("o"));
        assert!(
            arrive(&mut presenter, frame, now)
                .expect("a held step")
                .is_empty()
        );
        assert_eq!(presenter.deadline(), Some(ms(now, 250)));
        let late = admission.run();
        let mut effects = Vec::new();
        if tick_first {
            effects.extend(presenter.tick(ms(now, 250)).expect("a tick"));
            effects.extend(
                presenter
                    .completed(late, ms(now, 250))
                    .expect("a stale answer"),
            );
        } else {
            effects.extend(
                presenter
                    .completed(late, ms(now, 250))
                    .expect("a late answer"),
            );
            effects.extend(presenter.tick(ms(now, 250)).expect("a tick"));
        }
        let case = format!("tick first {tick_first}");
        assert_eq!(
            controls(&effects),
            [&ControlToBrain::SceneFailed { scene: LIVE }],
            "{case}"
        );
        assert_eq!(credit(&effects), FRAME, "{case}");
        assert_eq!(
            admissions(effects).len(),
            1,
            "{case}: the held commit is next"
        );
        assert_eq!(
            (presenter.global.usage(), presenter.residency()),
            alone,
            "{case}"
        );
        assert!(!presenter.scenes.contains_key(&LIVE), "{case}");
        assert!(presenter.cleared.contains(&LIVE), "{case}");
        assert_eq!(presenter.workers(), 0, "{case}");
    }
}

#[test]
fn a_step_admitted_after_its_predecessor_is_ready_keeps_the_predecessor_s_time() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    let frame = commit_frame(LIVE, 1, rect("b"));
    let admission = only_admission(arrive(&mut presenter, frame, ms(now, 100)).expect("a step"));
    let effects = presenter
        .completed(first.run(&mut Shaper), ms(now, 200))
        .expect("the predecessor");
    assert!(jobs(effects).is_empty());
    let effects = presenter
        .completed(admission.run(), ms(now, 240))
        .expect("the admission");
    assert_eq!(only_job(effects).scene(), LIVE);
    assert_eq!(presenter.deadline(), Some(ms(now, 450)));
}

#[test]
fn a_stop_before_or_during_admission_installs_the_step_unprepared_and_its_cut_applies() {
    for dispatched in [true, false] {
        let (mut presenter, now, mut zombies) = if dispatched {
            let (presenter, now) = live();
            (presenter, now, Vec::new())
        } else {
            exhausted()
        };
        begin(&mut presenter, LIVE, now);
        let frame = commit_frame(LIVE, 0, rect("a"));
        let mut sent = admissions(arrive(&mut presenter, frame, now).expect("a step"));
        let stopped = presenter.stop(RUN, ms(now, 1)).expect("a stop");
        let cut = SceneCut {
            scene: LIVE,
            cut: zero_cut(),
        };
        assert_eq!(controls(&stopped), [&stop_scenes(vec![cut])]);
        let admission = if dispatched {
            assert_eq!(sent.len(), 1);
            sent.remove(0)
        } else {
            assert!(sent.is_empty());
            let zombie = zombies.pop().expect("a held slot");
            let effects = presenter
                .completed(zombie.run(&mut Shaper), ms(now, 2))
                .expect("a stale answer");
            only_admission(effects)
        };
        let effects = presenter
            .completed(admission.run(), ms(now, 3))
            .expect("the admission");
        assert_eq!(credit(&effects), FRAME);
        assert!(controls(&effects).is_empty());
        assert!(jobs(effects).is_empty(), "a fenced step is never prepared");
        assert_eq!(presenter.scenes[&LIVE].state.view().revision(), 1);
        let effects =
            acknowledge(&mut presenter, LIVE, 1, zero_cut(), ms(now, 4)).expect("the cut");
        assert_eq!(credit(&effects), FRAME);
        assert!(!presenter.holds_scenes());
    }
}

/// A fenced held begin settles explicitly, even if the brain's unseen sweep names another run.
#[test]
fn a_scene_begun_behind_the_lane_at_stop_is_cut_once_by_the_brain() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let frame = commit_frame(LIVE, 0, rect("a"));
    let admission = only_admission(arrive(&mut presenter, frame, now).expect("a step"));
    let held = DataToShell::SceneBegin {
        scene: OTHER,
        header: header(),
    };
    assert!(
        arrive(&mut presenter, held, now)
            .expect("a held begin")
            .is_empty()
    );
    let frame = commit_frame(OTHER, 0, rect("o"));
    assert!(
        arrive(&mut presenter, frame, now)
            .expect("a held step")
            .is_empty()
    );
    let stopped = presenter.stop(RUN, ms(now, 1)).expect("a stop");
    let named = SceneCut {
        scene: LIVE,
        cut: zero_cut(),
    };
    assert_eq!(
        controls(&stopped),
        [&stop_scenes(vec![
            named,
            SceneCut {
                scene: OTHER,
                cut: zero_cut(),
            }
        ])]
    );
    let effects = presenter
        .completed(admission.run(), ms(now, 2))
        .expect("the admission");
    assert_eq!(credit(&effects), 2 * FRAME, "the commit and the held begin");
    assert!(controls(&effects).is_empty());
    let admission = only_admission(effects);
    let effects = presenter
        .completed(admission.run(), ms(now, 3))
        .expect("the held scene's admission");
    assert_eq!(credit(&effects), FRAME);
    assert!(
        controls(&effects).is_empty(),
        "the original Stop already named this scene"
    );
    assert!(jobs(effects).is_empty(), "a fenced step is never prepared");
    let other = &presenter.scenes[&OTHER];
    assert_eq!((other.cut, other.skipping), (Cut::Outstanding, true));
    for scene in [LIVE, OTHER] {
        let effects =
            acknowledge(&mut presenter, scene, 1, zero_cut(), ms(now, 4)).expect("the cut");
        assert_eq!(credit(&effects), FRAME, "{scene:?}");
        assert!(controls(&effects).is_empty(), "{scene:?}");
    }
    assert!(!presenter.holds_scenes());
    let late = acknowledge(&mut presenter, OTHER, 1, zero_cut(), ms(now, 5)).expect("a stale ack");
    assert_eq!(credit(&late), FRAME);
    assert!(controls(&late).is_empty());
    let later = RunId(RUN.0 + 1);
    let late = commit_as(&mut presenter, later, OTHER, 1, rect("p"), ms(now, 5)).expect("cleared");
    assert_eq!(credit(&late), FRAME);
    assert!(controls(&late).is_empty());
    assert!(jobs(late).is_empty());
    let fresh = SceneId(22);
    begin(&mut presenter, fresh, ms(now, 6));
    let effects =
        commit_as(&mut presenter, later, fresh, 0, rect("q"), ms(now, 6)).expect("a new run");
    assert_eq!(only_job(effects).scene(), fresh);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one stop sequence over the held scenes and the later run"
)]
fn older_held_scenes_settle_under_the_newest_stop_without_losing_a_later_run() {
    let newest = RunId(6);
    let third = SceneId(22);
    let fresh = SceneId(23);
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 0, rect("a")), now).expect("A admission"),
    );
    for (scene, run, id) in [(OTHER, RUN, "b"), (third, newest, "c")] {
        assert!(
            arrive(
                &mut presenter,
                DataToShell::SceneBegin {
                    scene,
                    header: header()
                },
                now
            )
            .expect("held begin")
            .is_empty()
        );
        assert!(
            arrive(
                &mut presenter,
                DataToShell::SceneStepCommit {
                    run,
                    scene,
                    index: 0,
                    step: rect(id),
                    speech: None,
                },
                now
            )
            .expect("held commit")
            .is_empty()
        );
    }
    assert_eq!(presenter.held.len(), 4);
    assert_eq!(presenter.held_bytes, 4 * FRAME);
    let effects = presenter.stop(newest, ms(now, 1)).expect("covering Stop");
    assert_eq!(
        controls(&effects),
        [&ControlToBrain::StopScenes {
            run: newest,
            cuts: SceneCuts::new(
                [LIVE, OTHER, third]
                    .map(|scene| SceneCut {
                        scene,
                        cut: zero_cut()
                    })
                    .to_vec()
            )
            .expect("three scenes"),
        }]
    );
    assert_eq!(presenter.fenced, Some(newest));
    assert_eq!(presenter.workers(), 1);
    let repeated = presenter
        .stop(RUN, ms(now, 1))
        .expect("older duplicate Stop before drain");
    assert_eq!(
        controls(&repeated),
        [&ControlToBrain::StopScenes {
            run: newest,
            cuts: SceneCuts::default()
        }]
    );
    let effects = presenter
        .completed(admission.run(), ms(now, 2))
        .expect("fenced A");
    assert_eq!(credit(&effects), 2 * FRAME);
    assert!(controls(&effects).is_empty());
    let admission = only_admission(effects);
    let effects = presenter
        .completed(admission.run(), ms(now, 3))
        .expect("fenced older B");
    assert!(
        controls(&effects).is_empty(),
        "older B was named before any later run"
    );
    assert_eq!(credit(&effects), 2 * FRAME);
    assert_eq!(
        (
            presenter.scenes[&OTHER].cut,
            presenter.scenes[&OTHER].skipping
        ),
        (Cut::Outstanding, true)
    );
    let admission = only_admission(effects);
    let effects = presenter
        .completed(admission.run(), ms(now, 4))
        .expect("fenced newer C");
    assert!(
        controls(&effects).is_empty(),
        "C has no duplicate explicit or unseen cut"
    );
    assert_eq!(credit(&effects), FRAME);
    assert!(jobs(effects).is_empty());
    assert_eq!(presenter.workers(), 0);
    assert_eq!(presenter.deadline(), None);
    assert!(presenter.held.is_empty());
    assert_eq!(presenter.held_bytes, 0);
    for scene in [LIVE, OTHER, third] {
        let held = &presenter.scenes[&scene];
        assert_eq!(
            (
                held.cut,
                held.skipping,
                held.state.view().revision(),
                held.state.view().activated()
            ),
            (Cut::Outstanding, true, 1, 0)
        );
        assert!(held.ready.is_empty());
    }
    let repeated = presenter.stop(newest, ms(now, 4)).expect("duplicate Stop");
    assert_eq!(
        controls(&repeated),
        [&ControlToBrain::StopScenes {
            run: newest,
            cuts: SceneCuts::default()
        }]
    );
    begin(&mut presenter, fresh, ms(now, 5));
    let later = only_job(
        commit_as(
            &mut presenter,
            RunId(7),
            fresh,
            0,
            rect("later"),
            ms(now, 5),
        )
        .expect("valid later run"),
    );
    for scene in [LIVE, OTHER, third] {
        let effects = acknowledge(&mut presenter, scene, 1, zero_cut(), ms(now, 6))
            .expect("FIFO cut settlement");
        assert_eq!(credit(&effects), FRAME);
        assert!(controls(&effects).is_empty());
    }
    assert_eq!(
        presenter.scenes.keys().copied().collect::<Vec<_>>(),
        [fresh]
    );
    assert_eq!(
        presenter.workers(),
        1,
        "older cuts do not cancel the later worker"
    );
    assert!(
        presenter
            .completed(later.run(&mut Shaper), ms(now, 6))
            .expect("later prepared")
            .is_empty()
    );
    assert_eq!(presenter.scenes[&fresh].ready.len(), 1);
    let late = acknowledge(&mut presenter, OTHER, 1, zero_cut(), ms(now, 7))
        .expect("duplicate old acknowledgement");
    assert!(controls(&late).is_empty());
    assert_eq!(presenter.scenes[&fresh].ready.len(), 1);
    let effects = presenter.clear(ms(now, 8)).expect("clear later scene");
    assert_eq!(controls(&effects), [&clear_drawing(vec![fresh])]);
    assert_eq!(presenter.residency(), 0);
    assert_eq!(presenter.global.usage(), Usage::default());
}

#[test]
fn second_clear_cancels_a_held_scenes_admission_and_late_cut_cannot_restore_it() {
    let third = SceneId(22);
    let newest = RunId(6);
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 0, rect("a")), now).expect("A admission"),
    );
    for (scene, run, id) in [(OTHER, RUN, "b"), (third, newest, "c")] {
        arrive(
            &mut presenter,
            DataToShell::SceneBegin {
                scene,
                header: header(),
            },
            now,
        )
        .expect("held begin");
        arrive(
            &mut presenter,
            DataToShell::SceneStepCommit {
                run,
                scene,
                index: 0,
                step: rect(id),
                speech: None,
            },
            now,
        )
        .expect("held commit");
    }
    presenter.stop(newest, ms(now, 1)).expect("first Stop");
    let admission = only_admission(presenter.completed(admission.run(), ms(now, 2)).expect("A"));
    let effects = presenter
        .completed(admission.run(), ms(now, 3))
        .expect("older B");
    assert!(controls(&effects).is_empty());
    assert_eq!(presenter.scenes[&OTHER].cut, Cut::Outstanding);
    let late = only_admission(effects);
    assert_eq!(presenter.workers(), 1);
    let effects = presenter.clear(ms(now, 4)).expect("second clear");
    assert_eq!(
        controls(&effects),
        [&clear_drawing(vec![LIVE, OTHER, third])]
    );
    assert_eq!(
        credit(&effects),
        FRAME,
        "C's in-flight credit is returned once"
    );
    assert_eq!(presenter.residency(), 0);
    assert_eq!(presenter.workers(), 1, "cancel is not worker completion");
    let effects = presenter
        .completed(late.run(), ms(now, 5))
        .expect("late canceled admission");
    assert_eq!(credit(&effects), 0);
    assert!(controls(&effects).is_empty());
    assert!(jobs(effects).is_empty());
    assert_eq!(presenter.workers(), 0);
    for scene in [LIVE, OTHER, third] {
        let effects =
            acknowledge(&mut presenter, scene, 1, zero_cut(), ms(now, 6)).expect("late cut");
        assert_eq!(credit(&effects), FRAME);
        assert!(controls(&effects).is_empty());
    }
    assert!(!presenter.holds_scenes());
    assert_eq!(presenter.global.usage(), Usage::default());
}

#[test]
fn stop_projects_held_removals_and_replacements_within_the_four_scene_bound() {
    let old = [LIVE, OTHER, SceneId(22), SceneId(23)];
    let replacement = SceneId(24);
    let (mut presenter, now) = live();
    for scene in old {
        begin(&mut presenter, scene, now);
    }
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 0, rect("a")), now).expect("admission"),
    );
    arrive(
        &mut presenter,
        DataToShell::ScenesRemoved {
            scenes: vec![OTHER],
        },
        now,
    )
    .expect("held removal");
    arrive(
        &mut presenter,
        DataToShell::SceneBegin {
            scene: replacement,
            header: header(),
        },
        now,
    )
    .expect("held replacement");
    let effects = presenter
        .stop(RUN, ms(now, 1))
        .expect("four identities after FIFO removal");
    assert_eq!(
        controls(&effects),
        [&stop_scenes(
            [LIVE, SceneId(22), SceneId(23), replacement]
                .map(|scene| SceneCut {
                    scene,
                    cut: zero_cut()
                })
                .to_vec()
        )]
    );
    assert!(matches!(
        effects.last(),
        Some(Effect::Control(ControlToBrain::StopScenes { .. }))
    ));
    let effects = presenter
        .completed(admission.run(), ms(now, 2))
        .expect("admitted before removal");
    assert_eq!(credit(&effects), 3 * FRAME);
    assert!(controls(&effects).is_empty());
    assert!(jobs(effects).is_empty());
    assert!(!presenter.scenes.contains_key(&OTHER));
    assert_eq!(
        presenter.scenes[&replacement].cut,
        Cut::Outstanding,
        "a held header already names a durable brain scene"
    );
    let cleared = presenter.clear(ms(now, 3)).expect("clear every survivor");
    assert_eq!(
        controls(&cleared),
        [&clear_drawing(vec![
            LIVE,
            SceneId(22),
            SceneId(23),
            replacement
        ])]
    );
    assert_eq!(presenter.residency(), 0);

    let (mut presenter, now) = live();
    for scene in old {
        begin(&mut presenter, scene, now);
    }
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 0, rect("a")), now).expect("admission"),
    );
    arrive(
        &mut presenter,
        DataToShell::SceneBegin {
            scene: replacement,
            header: header(),
        },
        now,
    )
    .expect("held fifth scene");
    assert!(matches!(
        presenter.stop(RUN, ms(now, 1)),
        Err(Broken::Scenes)
    ));
    presenter.disconnected();
    assert!(
        presenter
            .completed(admission.run(), ms(now, 2))
            .expect("late answer")
            .is_empty()
    );
    assert_eq!(presenter.workers(), 0);
    assert_eq!(presenter.residency(), 0);
}

#[test]
fn an_admission_past_the_global_object_cap_at_install_ends_the_connection_unchanged() {
    let (mut presenter, now) = live();
    let many = |count: usize| {
        let puts: Vec<_> = (0..count)
            .map(|i| format!(r#"{{"text":{{"id":"s{i}","at":[0,0],"text":""}}}}"#))
            .collect();
        super::step(&format!(
            r#"{{"step":{{"id":"many","put":[{}]}}}}"#,
            puts.join(",")
        ))
    };
    for scene in [SceneId(30), SceneId(31), SceneId(32)] {
        begin(&mut presenter, scene, now);
        drop(commit(&mut presenter, scene, 0, many(500), now).expect("within the global cap"));
    }
    assert_eq!(presenter.global.usage().objects, MAX_GLOBAL_LIVE_OBJECTS);
    assert_eq!(presenter.global.usage().segments, 0);
    begin(&mut presenter, LIVE, now);
    let before = presenter.global.usage();
    let frame = commit_frame(LIVE, 0, rect("over"));
    let done = only_admission(arrive(&mut presenter, frame, now).expect("a step")).run();
    let Completion::Admitted {
        result: Some(Ok(_)),
        ..
    } = &done
    else {
        panic!("the worker admits it against its scene alone");
    };
    assert_eq!(
        presenter.completed(done, ms(now, 1)).err(),
        Some(Broken::Step)
    );
    assert_eq!(presenter.global.usage(), before);
    assert_eq!(presenter.scenes[&LIVE].state.view().revision(), 0);
}

#[test]
fn a_clear_or_reconnect_during_admission_resolves_its_credit_once() {
    for clear in [true, false] {
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        let frame = commit_frame(LIVE, 0, rect("a"));
        let admission = only_admission(arrive(&mut presenter, frame, now).expect("a step"));
        let held = DataToShell::SceneBegin {
            scene: OTHER,
            header: header(),
        };
        assert!(
            arrive(&mut presenter, held, now)
                .expect("a held begin")
                .is_empty()
        );
        if clear {
            let effects = presenter.clear(ms(now, 1)).expect("a clear");
            assert_eq!(credit(&effects), 2 * FRAME);
            assert_eq!(controls(&effects), [&clear_drawing(vec![LIVE, OTHER])]);
        } else {
            presenter.connected(GENERATION + 1);
        }
        assert!(admission.cancelled(), "clear {clear}");
        assert!(presenter.scenes.is_empty(), "clear {clear}");
        assert!(presenter.held.is_empty(), "clear {clear}");
        assert_eq!(presenter.global.usage(), Usage::default(), "clear {clear}");
        let late = presenter
            .completed(admission.run(), ms(now, 2))
            .expect("a stale answer");
        assert!(late.is_empty(), "clear {clear}");
        assert_eq!(presenter.workers(), 0, "clear {clear}");
    }
}

#[test]
fn held_frames_and_the_lane_share_the_data_window() {
    for over in [0, 1] {
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        let frame = Deferred::new(commit_frame(LIVE, 0, rect("a")), 0);
        let effects = presenter
            .data(GENERATION, frame, DATA_WINDOW - FRAME, now)
            .expect("a step");
        let admission = only_admission(effects);
        let removal = DataToShell::ScenesRemoved {
            scenes: vec![OTHER],
        };
        let held = presenter.data(GENERATION, Deferred::new(removal, 0), FRAME + over, now);
        if over == 1 {
            assert_eq!(held.err(), Some(Broken::Window));
            continue;
        }
        assert!(held.expect("exactly the window").is_empty());
        let effects = presenter
            .completed(admission.run(), ms(now, 1))
            .expect("the admission");
        assert_eq!(
            credit(&effects),
            DATA_WINDOW,
            "each withheld byte is granted once"
        );
    }
}

#[test]
fn a_discarded_admission_or_lost_answer_keeps_its_payload_and_one_lease_off_the_caller() {
    let text = "x".repeat(400);
    for answered in [false, true] {
        let case = format!("answered {answered}");
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
        let ready = presenter
            .completed(first.run(&mut Shaper), now)
            .expect("an answer");
        assert!(ready.is_empty(), "{case}");
        let log = presenter.scenes[&LIVE].state.view().recovery_log();
        let SceneEvent::Step { step, .. } = &log[0] else {
            panic!("the mirrored step");
        };
        let probe = Arc::clone(step);
        let frame = commit_frame(LIVE, 1, label("b", &text));
        let job = only_admission(arrive(&mut presenter, frame, now).expect("a step"));
        let pending = if answered {
            let answer = job.run();
            assert!(
                matches!(
                    answer,
                    Completion::Admitted {
                        result: Some(Ok(_)),
                        ..
                    }
                ),
                "{case}"
            );
            Ok(answer)
        } else {
            Err(job)
        };
        assert_eq!(presenter.workers(), 1, "{case}: one lease");
        presenter.clear(ms(now, 1)).expect("a clear");
        assert_eq!(presenter.workers(), 1, "{case}: the job holds it");
        let marker = Arc::new(());
        drop(Deferred::new(Arc::clone(&marker), 0));
        assert!(drained(&marker, 1), "{case}: the release is disposed");
        let owners = Arc::strong_count(&probe);
        assert!(owners > 1, "{case}: the snapshot keeps the scene");
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        let release = hold_dropper();
        let ((), freed) = crate::allocation::releases_of_size(text.len(), || match pending {
            Ok(answer) => drop(sender.send(answer).expect_err("the recipient is gone")),
            Err(job) => drop(job),
        });
        assert_eq!(freed, 0, "{case}: no text payload frees on the caller");
        assert_eq!(Arc::strong_count(&probe), owners, "{case}: the read waits");
        assert_eq!(presenter.workers(), 0, "{case}: the lease frees once");
        drop(release);
        assert!(drained(&probe, 1), "{case}: the dropper frees the scene");
    }
}

#[test]
fn a_late_admission_on_a_recovered_scene_releases_its_rest_and_resends_its_clear() {
    let (mut presenter, now, job) = recovering(vec![(LIVE, rect("r")), (OTHER, rect("k"))]);
    let effects = presenter
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("the batch");
    assert!(controls(&effects).is_empty());
    let rest = presenter.scenes[&LIVE].retained;
    assert!(rest > 0, "the recovered rest is charged");
    let kept = presenter.scenes[&OTHER].residency();
    let picture = presenter.scenes[&LIVE].view_bytes + presenter.scenes[&LIVE].frame_bytes;
    assert_eq!(
        presenter.residency(),
        kept + rest + picture + super::super::Activation::BYTES
    );
    let both = presenter.global.usage();
    assert_eq!((both.objects, both.scenes), (2, 2));
    let frame = commit_frame(LIVE, 1, rect("b"));
    let late = only_admission(arrive(&mut presenter, frame, ms(now, 30)).expect("a step"));
    assert!(presenter.tick(ms(now, 279)).expect("a tick").is_empty());
    let effects = presenter.tick(ms(now, 280)).expect("a tick");
    assert_eq!(credit(&effects), FRAME);
    assert_eq!(
        controls(&effects),
        [&ControlToBrain::SceneFailed { scene: LIVE }]
    );
    assert_eq!(presenter.residency(), kept);
    let one = presenter.global.usage();
    assert_eq!((one.objects, one.scenes), (1, 1));
    presenter.connected(GENERATION + 1);
    let named = SceneDescriptorV1::new(LIVE, 1, 1, 1).expect("descriptor");
    let named = Bounded::new(vec![named]).expect("the brain still holds the scene");
    let effects = presenter
        .described(GENERATION + 1, &named, ms(now, 300))
        .expect("membership");
    assert_eq!(controls(&effects), [&clear_drawing(vec![LIVE])]);
    let stale = presenter
        .completed(late.run(), ms(now, 310))
        .expect("a stale answer");
    assert!(stale.is_empty());
    assert_eq!(presenter.workers(), 0);
}

#[test]
fn a_step_ready_at_200_admitted_at_240_and_started_at_260_is_timed_from_200() {
    let (mut presenter, now, mut zombies) = exhausted();
    let mut zombie = || zombies.pop().expect("a held slot").run(&mut Shaper);
    let freed = presenter.completed(zombie(), now).expect("a free slot");
    assert!(freed.is_empty());
    begin(&mut presenter, LIVE, now);
    begin(&mut presenter, OTHER, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    let frame = commit_frame(LIVE, 1, rect("b"));
    let waiting = arrive(&mut presenter, frame, now).expect("no slot");
    assert!(waiting.is_empty());
    let frame = commit_frame(OTHER, 0, rect("o"));
    let held = arrive(&mut presenter, frame, now).expect("a held step");
    assert!(held.is_empty());
    let effects = presenter
        .completed(first.run(&mut Shaper), ms(now, 200))
        .expect("the predecessor");
    let admission = only_admission(effects);
    let effects = presenter
        .completed(admission.run(), ms(now, 240))
        .expect("the admission");
    assert_eq!(credit(&effects), FRAME);
    let _other = only_admission(effects);
    assert_eq!(presenter.scenes[&LIVE].queue.len(), 1);
    assert_eq!(presenter.workers(), 8);
    let started = presenter
        .completed(zombie(), ms(now, 260))
        .expect("a freed slot");
    assert_eq!(only_job(started).scene(), LIVE);
    assert_eq!(presenter.deadline(), Some(ms(now, 450)));
    let early = presenter.tick(ms(now, 449)).expect("a tick");
    assert!(controls(&early).is_empty());
    let effects = presenter.tick(ms(now, 450)).expect("a tick");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 1, SceneFailure::Timeout)]
    );
}

#[test]
fn a_refused_consumption_drains_the_commit_held_behind_it() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    presenter
        .completed(first.run(&mut Shaper), now)
        .expect("prepared");
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 1, rect("b")), now).expect("admission"),
    );
    assert!(presenter.advance(GENERATION, now).expect("busy").is_empty());
    assert!(
        arrive(&mut presenter, commit_frame(LIVE, 2, rect("c")), now)
            .expect("held commit")
            .is_empty()
    );
    let prepared = only_job(presenter.completed(admission.run(), now).expect("admitted"));
    presenter
        .completed(prepared.run(&mut Shaper), now)
        .expect("prepared");
    assert_eq!(presenter.workers(), 0);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, Some(0));
    assert_eq!(presenter.held_bytes, FRAME);

    presenter
        .returned
        .store(MAX_RESIDENT_BYTES, Ordering::Relaxed);
    let mut effects = presenter
        .consumption_ready(GENERATION, LIVE, 0, now)
        .expect("refused for residency");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 0, SceneFailure::Residency)]
    );
    let position = effects
        .iter()
        .position(|effect| matches!(effect, Effect::Admit(_)))
        .expect("the held commit enters the lane in the same call");
    let Effect::Admit(admission) = effects.remove(position) else {
        unreachable!()
    };
    assert!(presenter.held.is_empty());
    assert_eq!(presenter.held_bytes, 0);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, None);
    let effects = presenter
        .completed(admission.run(), now)
        .expect("admitted after the failure");
    assert_eq!(credit(&effects), FRAME);
    assert!(
        jobs(effects).is_empty(),
        "the failed scene prepares nothing before its cut is acknowledged"
    );
}

#[test]
fn a_step_that_times_out_while_its_consumption_waits_drains_the_commit_held_behind_it() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    presenter
        .completed(first.run(&mut Shaper), now)
        .expect("prepared");
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 1, rect("b")), now).expect("admission"),
    );
    assert!(presenter.advance(GENERATION, now).expect("busy").is_empty());
    assert!(
        arrive(&mut presenter, commit_frame(LIVE, 2, rect("c")), now)
            .expect("held commit")
            .is_empty()
    );
    let _running = only_job(presenter.completed(admission.run(), now).expect("admitted"));
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, Some(0));
    assert_eq!(presenter.held_bytes, FRAME);
    assert_eq!(presenter.deadline(), Some(ms(now, 250)));

    let early = presenter.tick(ms(now, 249)).expect("a tick");
    assert!(controls(&early).is_empty());
    assert_eq!(presenter.held_bytes, FRAME);
    let mut effects = presenter.tick(ms(now, 250)).expect("a tick");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 1, SceneFailure::Timeout)]
    );
    let position = effects
        .iter()
        .position(|effect| matches!(effect, Effect::Admit(_)))
        .expect("the held commit enters the lane in the same tick");
    let Effect::Admit(admission) = effects.remove(position) else {
        unreachable!()
    };
    assert!(presenter.held.is_empty());
    assert_eq!(presenter.held_bytes, 0);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, None);
    let effects = presenter
        .completed(admission.run(), ms(now, 250))
        .expect("admitted after the timeout");
    assert_eq!(credit(&effects), FRAME);
    assert!(
        jobs(effects).is_empty(),
        "the failed scene prepares nothing before its cut is acknowledged"
    );
}

#[test]
fn a_cut_acknowledgement_that_arrives_mid_lane_waits_for_the_lane() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    presenter
        .completed(first.run(&mut Shaper), now)
        .expect("prepared");
    presenter.stop(RUN, now).expect("stop");
    assert_eq!(presenter.scenes[&LIVE].cut, Cut::Outstanding);
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 1, rect("late")), now).expect("a late step"),
    );
    let acknowledgement = DataToShell::SceneCutApplied {
        scene: LIVE,
        revision: 2,
        cut: zero_cut(),
    };
    assert!(
        arrive(&mut presenter, acknowledgement, now)
            .expect("held behind the lane")
            .is_empty(),
        "no credit and no cut while the lane is out"
    );
    assert_eq!(presenter.held_bytes, FRAME);
    assert_eq!(presenter.scenes[&LIVE].cut, Cut::Outstanding);
    let effects = presenter
        .completed(admission.run(), now)
        .expect("the lane, then the acknowledgement");
    assert_eq!(credit(&effects), 2 * FRAME);
    assert!(presenter.held.is_empty());
    assert!(
        !presenter.holds_scenes(),
        "the acknowledged cut at nothing activated clears the scene"
    );
}
