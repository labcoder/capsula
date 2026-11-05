//! The admission lane: one commit at a time, its deadline, Stop and clear around it, busy
//! consumption and the credit it withholds.

use std::sync::{Arc, mpsc};

use kumochi_protocol::message::{
    DATA_WINDOW, DataToShell, RunId, SceneCut, SceneDescriptorV1, SceneFailure, SceneId,
};
use kumochi_scene::{
    SceneEvent, budget::Usage, deferred::Deferred, limits::MAX_GLOBAL_LIVE_OBJECTS, text::Bounded,
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
            .consume_ready(LIVE, 0, ms(now, 3))
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
    let consumed = presenter.consume_ready(LIVE, 0, ms(now, 6)).expect("retry");
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
        .consume_ready(LIVE, 1, now)
        .expect_err("not the head");
    assert_eq!(refused, ConsumeError::OutOfOrder);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, None);
    let refused = presenter.consume_ready(LIVE, 0, now).expect_err("busy");
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
        .consume_ready(LIVE, 0, ms(now, 6))
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
        assert_eq!(controls(&effects), [&clear_drawing(vec![LIVE])], "{case}");
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

/// The brain cuts a stopped run's never-activated scenes that the Stop did not name, so a scene
/// begun behind the lane gets exactly that one cut, never a second one from the shell.
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
    assert_eq!(controls(&stopped), [&stop_scenes(vec![named])]);
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
    assert!(controls(&effects).is_empty(), "the brain owns this cut");
    assert!(jobs(effects).is_empty(), "a fenced step is never prepared");
    let other = &presenter.scenes[&OTHER];
    assert_eq!((other.cut, other.skipping), (Cut::Open, true));
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
    let control = text.clone().into_boxed_str();
    let ((), _, [_, freed]) = crate::allocation::usage(|| drop(control));
    assert_eq!(freed, text.len(), "calibration: a caller drop counts");
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
        crate::allocation::usage(|| match pending {
            Ok(answer) => drop(sender.send(answer).expect_err("the recipient is gone")),
            Err(job) => drop(job),
        });
        let largest = crate::allocation::largest_release();
        assert!(largest < text.len(), "{case}: freed {largest}");
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
    assert_eq!(presenter.residency(), kept + rest + picture);
    let both = presenter.global.usage();
    assert_eq!((both.objects, both.scenes), (2, 2));
    let frame = commit_frame(LIVE, 1, rect("b"));
    let late = only_admission(arrive(&mut presenter, frame, ms(now, 30)).expect("a step"));
    assert!(presenter.tick(ms(now, 279)).expect("a tick").is_empty());
    let effects = presenter.tick(ms(now, 280)).expect("a tick");
    assert_eq!(credit(&effects), FRAME);
    assert_eq!(controls(&effects), [&clear_drawing(vec![LIVE])]);
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
