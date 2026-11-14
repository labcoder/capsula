//! The read-ahead window, eligibility deadlines and head-only consumption.

use std::{
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

use kumochi_protocol::message::{ControlToBrain, DataToShell, RunId, SceneFailure, SceneId};
use kumochi_scene::{
    DetachedCandidate, Frame, SceneState, SceneView,
    deferred::{Deferred, Pending, pending},
    layout::LayoutState,
    limits::MAX_RESIDENT_BYTES,
};

use super::super::{scene::WINDOW, window::ConsumeError};
use super::{
    Charged, Completion, Effect, LIVE, PrepareJob, Presenter, Probed, RUN, Shaper, Witness,
    acknowledge, arrive, begin, bound_everywhere, commit, commit_as, commit_frame, controls,
    drained, exhausted, failed, hold_dropper, jobs, live, ms, only_admission, only_job, recovering,
    rect, settle,
};

const FRAME_CACHE_METADATA_BYTES: usize =
    size_of::<std::cell::RefCell<super::super::paint::Frames>>()
        + 2 * size_of::<usize>()
        + super::super::Activation::BYTES;
/// A scene's paint cache charge in the cap fixtures, and a different one staged later.
const CACHE: usize = 4_099;
const NEXT_CACHE: usize = 1_031;

struct Seeded {
    cache: usize,
    fixed: usize,
}

impl super::super::Stage for Seeded {
    fn stage(&self, _: &SceneView) -> Box<dyn super::super::SceneCache> {
        Box::new(Charged(self.cache))
    }

    fn fixed_bytes(&self) -> usize {
        self.fixed
    }
}

fn answer(presenter: &mut Presenter, job: PrepareJob, at: Instant) -> Vec<Effect> {
    presenter
        .completed(job.run(&mut Shaper), at)
        .expect("an answer")
}

fn step(index: u32) -> kumochi_scene::Step {
    rect(&format!("s{index}"))
}

/// A live scene with `count` steps committed and prepared until its window is full or done.
fn prepared(count: u32) -> (Presenter, Instant) {
    charged(count, 0)
}

/// [`prepared`], with every paint cache staged from now on charging `cache` bytes.
fn charged(count: u32, cache: usize) -> (Presenter, Instant) {
    let (mut presenter, now) = live();
    presenter.stage = Box::new(Charged(cache));
    begin(&mut presenter, LIVE, now);
    let mut running = Vec::new();
    for index in 0..count {
        running.extend(jobs(
            commit(&mut presenter, LIVE, index, step(index), now).expect("a step"),
        ));
    }
    while let Some(job) = running.pop() {
        running.extend(jobs(answer(&mut presenter, job, now)));
    }
    (presenter, now)
}

/// A candidate admitted at the scene's current stamps, its snapshot already destroyed.
fn probe(presenter: &Presenter) -> DetachedCandidate {
    let candidate = presenter.scenes[&LIVE]
        .state
        .read()
        .admit_detached(&rect("probe"))
        .expect("a new probe");
    let marker = Arc::new(());
    drop(Deferred::new(Arc::clone(&marker), 0));
    assert!(drained(&marker, 1), "the dropper destroyed the snapshot");
    candidate
}

/// Installs a probe, which succeeds only while the scene keeps the stamps it was admitted at.
fn install(presenter: &mut Presenter, probe: DetachedCandidate, unchanged: &str) {
    let scene = presenter.scenes.get_mut(&LIVE).expect("the scene");
    let read = scene.state.read();
    scene
        .state
        .apply_detached(read, probe, &mut presenter.global)
        .expect(unchanged);
}

fn positions(presenter: &Presenter) -> usize {
    let scene = &presenter.scenes[&LIVE];
    scene.ready.len() + usize::from(scene.running.is_some())
}

/// What a refused consumption or adoption must leave alone: budget, semantic state and charge.
fn semantic(presenter: &Presenter) -> impl std::fmt::Debug + PartialEq + use<> {
    let state = presenter.scenes[&LIVE].state.view();
    (
        presenter.global.generation(),
        presenter.global.usage(),
        state.revision(),
        state.activated(),
        state.attributed_bytes(),
    )
}

fn shape(presenter: &Presenter) -> impl std::fmt::Debug + PartialEq + use<> {
    let scene = &presenter.scenes[&LIVE];
    (
        semantic(presenter),
        scene.ready.len(),
        scene.queue.len(),
        scene.running.is_some(),
        scene.retained,
        presenter.residency(),
        scene.view.activated(),
        scene.view_bytes,
        scene.cache_bytes,
        presenter.frames.borrow().epoch(LIVE),
        scene.pending_consumption,
    )
}

#[test]
fn eight_steps_fill_the_window_and_a_ninth_waits_untimed() {
    for count in [8, 9] {
        let (mut presenter, now) = prepared(count);
        let scene = &presenter.scenes[&LIVE];
        assert_eq!(scene.ready.len(), WINDOW, "{count} steps");
        assert!(scene.running.is_none(), "{count} steps");
        assert_eq!(
            scene.queue.len(),
            usize::try_from(count).expect("small") - WINDOW
        );
        assert_eq!(presenter.deadline(), None, "{count} steps");
        let effects = presenter.tick(ms(now, 10_000)).expect("a tick");
        assert!(effects.is_empty(), "{count} steps");
        assert_eq!(
            presenter.scenes[&LIVE].queue.len(),
            usize::try_from(count).expect("small") - WINDOW
        );
    }
}

#[test]
fn a_consumption_frees_one_position_and_starts_the_waiting_step_from_then() {
    let (mut presenter, now) = prepared(10);
    let first = presenter
        .consume_ready(LIVE, 0, ms(now, 10_000), 0, 0)
        .expect("the head");
    let started = jobs(first.effects.expect("a current scene"));
    assert_eq!(started.len(), 1);
    assert_eq!(positions(&presenter), WINDOW);
    assert_eq!(presenter.deadline(), Some(ms(now, 10_250)));
    let second = presenter
        .consume_ready(LIVE, 1, ms(now, 10_100), 0, 0)
        .expect("the next head");
    assert!(jobs(second.effects.expect("a current scene")).is_empty());
    assert_eq!(positions(&presenter), WINDOW - 1);
    assert_eq!(presenter.deadline(), Some(ms(now, 10_250)));
    drop(started);
}

#[test]
fn a_step_started_by_a_freed_position_is_timed_from_that_consumption() {
    for (at, tick) in [
        (10_249, false),
        (10_250, false),
        (10_249, true),
        (10_250, true),
    ] {
        let case = format!("{at} ms, tick {tick}");
        let (mut presenter, now) = prepared(9);
        let done = presenter
            .consume_ready(LIVE, 0, ms(now, 10_000), 0, 0)
            .expect("the head");
        let job = only_job(done.effects.expect("a current scene"));
        let cut = presenter.scenes[&LIVE].cut_now();
        let effects = if tick {
            presenter.tick(ms(now, at)).expect("a tick")
        } else {
            answer(&mut presenter, job, ms(now, at))
        };
        let late = ControlToBrain::StepFailed {
            run: RUN,
            scene: LIVE,
            index: 8,
            reason: SceneFailure::Timeout,
            cut,
        };
        let (failures, ready): (&[&ControlToBrain], _) = match (at, tick) {
            (10_249, false) => (&[], WINDOW),
            (10_249, true) => (&[], WINDOW - 1),
            _ => (&[&late], 0),
        };
        assert_eq!(controls(&effects), failures, "{case}");
        assert_eq!(presenter.scenes[&LIVE].ready.len(), ready, "{case}");
    }
}

#[test]
fn a_scene_longer_than_the_window_is_consumed_through_its_end() {
    for count in [9, 20] {
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        let mut running = Vec::new();
        for index in 0..count {
            running.extend(jobs(
                commit(&mut presenter, LIVE, index, step(index), now).expect("a step"),
            ));
        }
        let (mut consumed, mut widest) = (0, 0);
        while consumed < count {
            widest = widest.max(positions(&presenter));
            assert!(positions(&presenter) <= WINDOW, "{count} steps");
            if let Some(job) = running.pop() {
                running.extend(jobs(answer(&mut presenter, job, now)));
                continue;
            }
            let done = presenter
                .consume_ready(LIVE, consumed, now, 0, 0)
                .expect("the head");
            running.extend(jobs(done.effects.expect("a current scene")));
            assert_eq!(done.step.step().index, consumed, "{count} steps");
            drop(done.step);
            consumed += 1;
        }
        assert_eq!(widest, WINDOW, "{count} steps");
        let scene = &presenter.scenes[&LIVE];
        assert_eq!(scene.state.view().activated(), count, "{count} steps");
        assert!(
            scene.ready.is_empty() && scene.queue.is_empty(),
            "{count} steps"
        );
        assert_eq!(
            presenter.returned.load(Ordering::Relaxed),
            0,
            "{count} steps"
        );
        assert_eq!(
            presenter.residency(),
            scene.rest.resident_bytes()
                + scene.view_bytes
                + scene.frame_bytes
                + super::super::Activation::BYTES,
            "{count} steps"
        );
    }
}

#[test]
fn refused_consumptions_change_nothing() {
    let (mut presenter, now) = prepared(2);
    let before = shape(&presenter);
    for (scene, index, error) in [
        (SceneId(99), 0, ConsumeError::UnknownScene),
        (LIVE, 1, ConsumeError::OutOfOrder),
        (LIVE, 7, ConsumeError::OutOfOrder),
    ] {
        let refused = presenter
            .consume_ready(scene, index, now, 0, 0)
            .expect_err("refused");
        assert_eq!(refused, error);
        assert_eq!(shape(&presenter), before, "{error:?}");
    }
    drop(
        presenter
            .consume_ready(LIVE, 0, now, 0, 0)
            .expect("the head"),
    );
    let after = shape(&presenter);
    let refused = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect_err("consumed");
    assert_eq!(refused, ConsumeError::AlreadyConsumed);
    assert_eq!(shape(&presenter), after);
    drop(
        presenter
            .consume_ready(LIVE, 1, now, 0, 0)
            .expect("the last head"),
    );
    let after = shape(&presenter);
    let refused = presenter
        .consume_ready(LIVE, 2, now, 0, 0)
        .expect_err("nothing ready");
    assert_eq!(refused, ConsumeError::NotReady);
    assert_eq!(shape(&presenter), after);
}

#[test]
fn the_head_s_full_layout_becomes_the_rest_under_its_running_successor() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, step(0), now).expect("a step"));
    assert!(jobs(commit(&mut presenter, LIVE, 1, step(1), now).expect("a step")).is_empty());
    let second = only_job(answer(&mut presenter, first, now));
    let head = &presenter.scenes[&LIVE].ready[0];
    let (full_after, owner_bytes) = (head.full_after, head.step.attribution());
    let done = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect("the head");
    let scene = &presenter.scenes[&LIVE];
    assert_eq!(scene.retained, full_after);
    assert_eq!(scene.rest.resident_bytes(), full_after);
    assert_eq!(presenter.returned.load(Ordering::Relaxed), owner_bytes);
    let effects = answer(&mut presenter, second, ms(now, 10));
    assert!(controls(&effects).is_empty());
    assert_eq!(presenter.scenes[&LIVE].ready.len(), 1);
    drop(done);
    assert_eq!(presenter.returned.load(Ordering::Relaxed), 0);
}

#[test]
fn a_cut_s_revision_gap_does_not_hold_back_the_next_step() {
    let (mut presenter, now) = prepared(20);
    drop(
        presenter
            .consume_ready(LIVE, 0, now, 0, 0)
            .expect("the head"),
    );
    let cut = presenter.scenes[&LIVE].cut_now();
    presenter.stop(RUN, now).expect("a stop");
    acknowledge(&mut presenter, LIVE, 20, cut, now).expect("the cut");
    let job =
        only_job(commit_as(&mut presenter, RunId(6), LIVE, 20, rect("t"), now).expect("a step"));
    assert!(answer(&mut presenter, job, now).is_empty());
    for (index, error) in [
        (0, ConsumeError::AlreadyConsumed),
        (19, ConsumeError::OutOfOrder),
    ] {
        let refused = presenter
            .consume_ready(LIVE, index, now, 0, 0)
            .expect_err("refused");
        assert_eq!(refused, error, "index {index}");
    }
    let done = presenter
        .consume_ready(LIVE, 20, now, 0, 0)
        .expect("the first new successor");
    assert_eq!(done.step.step().index, 20);
    assert_eq!(presenter.scenes[&LIVE].state.view().activated(), 21);
}

#[test]
fn a_step_eligible_while_every_slot_is_held_spends_its_wait_from_the_same_deadline() {
    for at in [250, 251] {
        let (mut presenter, now, mut zombies) = exhausted();
        begin(&mut presenter, LIVE, now);
        assert!(jobs(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step")).is_empty());
        let zombie = zombies.pop().expect("a held slot");
        let effects = answer(&mut presenter, zombie, ms(now, 249));
        let job = only_job(settle(&mut presenter, effects, ms(now, 249)).expect("an admission"));
        assert_eq!(presenter.deadline(), Some(ms(now, 250)));
        let effects = answer(&mut presenter, job, ms(now, at));
        assert_eq!(
            controls(&effects),
            [&failed(LIVE, 0, SceneFailure::Timeout)],
            "an answer at {at} ms"
        );
        assert!(
            presenter.scenes[&LIVE].ready.is_empty(),
            "an answer at {at} ms"
        );
    }
}

#[test]
fn the_watchdog_and_a_late_answer_fail_a_step_once_in_either_order() {
    for tick_first in [true, false] {
        let (mut presenter, now, mut zombies) = exhausted();
        begin(&mut presenter, LIVE, now);
        commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step");
        let zombie = zombies.pop().expect("a held slot");
        let effects = answer(&mut presenter, zombie, ms(now, 249));
        let job = only_job(settle(&mut presenter, effects, ms(now, 249)).expect("an admission"));
        let done = job.run(&mut Shaper);
        let mut effects = Vec::new();
        if tick_first {
            effects.extend(presenter.tick(ms(now, 250)).expect("a tick"));
            effects.extend(presenter.completed(done, ms(now, 250)).expect("an answer"));
        } else {
            effects.extend(presenter.completed(done, ms(now, 250)).expect("an answer"));
            effects.extend(presenter.tick(ms(now, 250)).expect("a tick"));
        }
        assert_eq!(
            controls(&effects),
            [&failed(LIVE, 0, SceneFailure::Timeout)],
            "tick first {tick_first}"
        );
        assert!(
            presenter.scenes[&LIVE].ready.is_empty(),
            "tick first {tick_first}"
        );
    }
}

#[test]
fn a_halt_frees_its_positions_once_and_a_late_answer_takes_none_back() {
    for clear in [false, true] {
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        let first = only_job(commit(&mut presenter, LIVE, 0, step(0), now).expect("a step"));
        commit(&mut presenter, LIVE, 1, step(1), now).expect("a step");
        let second = only_job(answer(&mut presenter, first, now));
        assert_eq!(positions(&presenter), 2);
        if clear {
            presenter.clear(now).expect("a clear");
            assert!(presenter.scenes.is_empty());
        } else {
            presenter.stop(RUN, now).expect("a stop");
            assert_eq!(positions(&presenter), 0);
        }
        assert_eq!(presenter.workers(), 1, "clear {clear}");
        let late = answer(&mut presenter, second, ms(now, 10));
        assert!(late.is_empty(), "clear {clear}");
        assert_eq!(presenter.workers(), 0, "clear {clear}");
        if !clear {
            assert_eq!(positions(&presenter), 0);
        }
    }
}

#[test]
fn a_result_adopts_at_exactly_the_cap_and_fails_one_byte_over() {
    for over in [0, 1] {
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        let done = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"))
            .run(&mut Shaper);
        let Completion::Prepared { result, .. } = &done else {
            panic!("a prepared step");
        };
        let Ok(prepared) = &**result else {
            panic!("a prepared step");
        };
        let delta = prepared.resident_bytes;
        let held = MAX_RESIDENT_BYTES - presenter.residency() - delta + over;
        presenter.returned.store(held, Ordering::Relaxed);
        let stamp = probe(&presenter);
        let before = semantic(&presenter);
        let effects = presenter.completed(done, now).expect("an answer");
        assert_eq!(semantic(&presenter), before, "{over} over");
        if over == 0 {
            assert!(effects.is_empty());
            assert_eq!(presenter.residency(), MAX_RESIDENT_BYTES);
        } else {
            assert_eq!(
                controls(&effects),
                [&failed(LIVE, 0, SceneFailure::Residency)]
            );
            assert!(presenter.scenes[&LIVE].ready.is_empty());
        }
        install(
            &mut presenter,
            stamp,
            "adoption leaves the mutation stamp unchanged",
        );
        presenter.returned.store(0, Ordering::Relaxed);
    }
}

#[test]
fn a_new_empty_rest_also_respects_returned_owners_at_the_cap() {
    for fixed in [0, 17_411] {
        for over in [0, 1] {
            let (mut presenter, now) = live();
            let rest = kumochi_scene::layout::LayoutState::new(
                &kumochi_scene::SceneState::begin(super::header().begin).expect("the header"),
            );
            let empty = rest.resident_bytes()
                + kumochi_scene::SceneView::new(&rest, 0).resident_bytes()
                + kumochi_scene::Frame::new().resident_bytes()
                + FRAME_CACHE_METADATA_BYTES
                + CACHE;
            presenter.stage = Box::new(Seeded {
                cache: CACHE,
                fixed,
            });
            presenter
                .returned
                .store(MAX_RESIDENT_BYTES - empty - fixed + over, Ordering::Relaxed);
            let before = (presenter.global.usage(), presenter.global.generation());
            let effects = begin(&mut presenter, LIVE, now);
            assert_eq!(
                (presenter.global.usage(), presenter.global.generation()),
                before
            );
            if over == 0 {
                assert!(controls(&effects).is_empty());
                assert_eq!(presenter.scenes.len(), 1);
                assert_eq!(presenter.scenes[&LIVE].cache_bytes, CACHE);
                assert_eq!(presenter.residency(), MAX_RESIDENT_BYTES);
            } else {
                assert_eq!(
                    controls(&effects),
                    [&ControlToBrain::SceneFailed { scene: LIVE }]
                );
                assert!(presenter.scenes.is_empty());
                assert_eq!(presenter.residency(), MAX_RESIDENT_BYTES - empty + 1);
            }
            presenter.returned.store(0, Ordering::Relaxed);
            if over == 1 {
                assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
                assert!(
                    presenter.scenes.is_empty(),
                    "a later valid begin cannot undo failure"
                );
                assert_eq!(presenter.residency(), fixed);
            }
        }
    }
}

#[test]
fn recovery_checks_its_view_and_frame_at_the_cap() {
    for fixed in [0, 17_411] {
        for over in [0, 1] {
            let (mut presenter, now, job) = recovering(vec![(LIVE, rect("r"))]);
            presenter.stage = Box::new(Seeded {
                cache: CACHE,
                fixed,
            });
            let done = job.run(&mut Shaper);
            let Completion::Recovered { scenes, .. } = &done else {
                panic!("a recovery answer");
            };
            let (state, rest) = scenes[0].1.as_ref().expect("the recovered scene");
            let picture = scenes[0].2
                + kumochi_scene::SceneView::new(rest, state.activated()).resident_bytes()
                + kumochi_scene::Frame::new().resident_bytes()
                + FRAME_CACHE_METADATA_BYTES
                + CACHE;
            let returned = MAX_RESIDENT_BYTES - picture - fixed + over;
            presenter.returned.store(returned, Ordering::Relaxed);
            let effects = presenter.completed(done, now).expect("recovery adoption");
            if over == 0 {
                assert!(controls(&effects).is_empty());
                assert_eq!(presenter.scenes.len(), 1);
                assert_eq!(presenter.scenes[&LIVE].cache_bytes, CACHE);
                assert_eq!(presenter.residency(), MAX_RESIDENT_BYTES);
            } else {
                assert_eq!(
                    controls(&effects),
                    [&ControlToBrain::ClearDrawing {
                        scene_ids: vec![LIVE],
                    }]
                );
                assert!(presenter.scenes.is_empty());
                assert_eq!(presenter.residency(), returned + fixed);
            }
            presenter.returned.store(0, Ordering::Relaxed);
            if over == 1 {
                assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
                assert!(
                    presenter.scenes.is_empty(),
                    "a later valid begin cannot undo failure"
                );
                assert_eq!(presenter.residency(), fixed);
            }
        }
    }
}

#[test]
fn a_kept_cut_checks_its_replacement_view_at_the_cap() {
    for fixed in [0, 17_411] {
        for over in [0, 1] {
            let (_reference, _, job) = recovering(vec![(LIVE, rect("r"))]);
            let reference = job.run(&mut Shaper);
            let Completion::Recovered { scenes, .. } = &reference else {
                panic!("a one-object reference");
            };
            let (state, rest) = scenes[0].1.as_ref().expect("the reference rest");
            let picture = scenes[0].2
                + kumochi_scene::SceneView::new(rest, state.activated()).resident_bytes()
                + kumochi_scene::Frame::new().resident_bytes()
                + FRAME_CACHE_METADATA_BYTES
                + NEXT_CACHE;
            let pair = super::step(
                r#"{"step":{"id":"pair","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10]}},{"shape":{"id":"b","kind":"rect","box":[0,0,10,10]}}]}}"#,
            );
            let (mut presenter, now, job) = recovering(vec![(LIVE, pair)]);
            presenter.stage = Box::new(Seeded {
                cache: CACHE,
                fixed,
            });
            presenter
                .completed(job.run(&mut Shaper), now)
                .expect("recovered rest");
            assert!(
                presenter.residency() > picture,
                "the cut removes one object's storage"
            );
            presenter.stage = Box::new(Seeded {
                cache: NEXT_CACHE,
                fixed,
            });
            presenter.stop(RUN, now).expect("a stop at rest");
            let cut = super::kept(LIVE).cut;
            let revision = presenter.scenes[&LIVE].state.view().revision();
            let returned = MAX_RESIDENT_BYTES - picture - fixed + over;
            presenter.returned.store(returned, Ordering::Relaxed);
            let effects = acknowledge(&mut presenter, LIVE, revision, cut, now).expect("the cut");
            if over == 0 {
                assert!(controls(&effects).is_empty());
                assert_eq!(presenter.scenes.len(), 1);
                assert_eq!(presenter.scenes[&LIVE].cache_bytes, NEXT_CACHE);
                assert_eq!(presenter.residency(), MAX_RESIDENT_BYTES);
            } else {
                assert_eq!(
                    controls(&effects),
                    [&ControlToBrain::ClearDrawing {
                        scene_ids: vec![LIVE],
                    }]
                );
                assert!(presenter.scenes.is_empty());
                assert_eq!(presenter.residency(), returned + fixed);
            }
            presenter.returned.store(0, Ordering::Relaxed);
            if over == 1 {
                assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
                assert!(
                    presenter.scenes.is_empty(),
                    "a later valid begin cannot undo failure"
                );
                assert_eq!(presenter.residency(), fixed);
            }
        }
    }
}

#[test]
fn a_new_scene_and_its_consumption_bind_caches_staged_from_their_views() {
    let (mut presenter, now) = live();
    presenter.stage = Box::new(Witness);
    begin(&mut presenter, LIVE, now);
    let (_, effects) = presenter.sample(0);
    assert!(effects.is_empty());
    assert!(bound_everywhere(&presenter), "the new scene's view");
    let effects = commit(&mut presenter, LIVE, 0, step(0), now).expect("a step");
    let mut running = jobs(effects);
    while let Some(job) = running.pop() {
        running.extend(jobs(answer(&mut presenter, job, now)));
    }
    let done = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect("the head");
    let (_, effects) = presenter.sample(0);
    assert!(effects.is_empty());
    assert!(bound_everywhere(&presenter), "the activated view");
    drop(done);
}

#[test]
fn every_outgoing_cache_waits_in_the_dropper() {
    let probe = Arc::new(());
    // The test's probe and the stage's own, plus one per cache alive or queued.
    let caches = |count: usize| 2 + count;
    let (mut presenter, now) = live();
    presenter.stage = Box::new(Probed(Arc::clone(&probe)));
    presenter
        .returned
        .store(MAX_RESIDENT_BYTES, Ordering::Relaxed);
    let release = hold_dropper();
    begin(&mut presenter, SceneId(21), now);
    assert!(presenter.scenes.is_empty(), "a scene over the cap");
    assert_eq!(Arc::strong_count(&probe), caches(1), "its staged cache");
    drop(release);
    assert!(drained(&probe, caches(0)));
    presenter.returned.store(0, Ordering::Relaxed);
    begin(&mut presenter, LIVE, now);
    let effects = commit(&mut presenter, LIVE, 0, step(0), now).expect("a step");
    let mut running = jobs(effects);
    while let Some(job) = running.pop() {
        running.extend(jobs(answer(&mut presenter, job, now)));
    }
    assert_eq!(Arc::strong_count(&probe), caches(1), "the scene's cache");
    presenter
        .returned
        .store(MAX_RESIDENT_BYTES, Ordering::Relaxed);
    let release = hold_dropper();
    let refused = presenter.consume_ready(LIVE, 0, now, 0, 0);
    assert_eq!(refused.err(), Some(ConsumeError::Residency));
    assert_eq!(Arc::strong_count(&probe), caches(2), "the candidate's");
    drop(release);
    assert!(drained(&probe, caches(1)));
    presenter.returned.store(0, Ordering::Relaxed);
    let release = hold_dropper();
    let done = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect("the head");
    assert_eq!(Arc::strong_count(&probe), caches(2), "the outgoing cache");
    drop(release);
    assert!(drained(&probe, caches(1)));
    drop(done);
    let release = hold_dropper();
    presenter.clear(now).expect("a global clear");
    assert_eq!(Arc::strong_count(&probe), caches(1), "the released scene's");
    drop(release);
    assert!(drained(&probe, caches(0)));
}

#[test]
fn a_consumption_fits_at_exactly_the_cap_and_is_refused_one_byte_over() {
    for fixed in [0, 17_411] {
        // The activated view's own charge, read from an identical scene after a real install.
        let installed = {
            let (mut presenter, now) = prepared(1);
            let done = presenter
                .consume_ready(LIVE, 0, now, 0, 0)
                .expect("an identical head");
            drop(done);
            presenter.scenes[&LIVE].view.resident_bytes()
        };
        for over in [1, 0] {
            let (mut presenter, now) = charged(1, CACHE);
            let without_fixed = presenter.residency();
            presenter.stage = Box::new(Seeded {
                cache: NEXT_CACHE,
                fixed,
            });
            assert_eq!(
                presenter.residency(),
                without_fixed + fixed,
                "a fixed seed is charged once before consumption"
            );
            let scene = &presenter.scenes[&LIVE];
            assert!(
                installed > scene.view_bytes,
                "the activated view outweighs the empty one, so omitting it would admit one over"
            );
            let head = &scene.ready[0];
            let outgoing = scene.retained + scene.view_bytes + CACHE + head.step.resident_bytes;
            let incoming = head.full_after + installed + NEXT_CACHE + head.step.attribution();
            let after = presenter.residency() - outgoing + incoming;
            presenter
                .returned
                .store(MAX_RESIDENT_BYTES - after + over, Ordering::Relaxed);
            let stamp = probe(&presenter);
            let before = shape(&presenter);
            let result = presenter.consume_ready(LIVE, 0, now, 0, 0);
            if over == 1 {
                assert_eq!(result.expect_err("over the cap"), ConsumeError::Residency);
                assert_eq!(shape(&presenter), before);
                install(
                    &mut presenter,
                    stamp,
                    "refusal leaves the mutation stamp unchanged",
                );
            } else {
                let done = result.expect("at the cap");
                assert_eq!(presenter.residency(), MAX_RESIDENT_BYTES);
                let scene = &presenter.scenes[&LIVE];
                assert_eq!((scene.view.activated(), scene.view_bytes), (1, installed));
                assert_eq!(scene.cache_bytes, NEXT_CACHE);
                drop(done);
            }
            presenter.returned.store(0, Ordering::Relaxed);
        }
    }
}

#[test]
fn a_consumed_step_stays_charged_until_its_owner_drops_it() {
    let (mut presenter, now) = prepared(2);
    let charge = |presenter: &Presenter, index: usize| {
        let ready = &presenter.scenes[&LIVE].ready[index];
        ready.step.attribution()
    };
    let first_charge = charge(&presenter, 0);
    let first = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect("the head");
    let second_charge = charge(&presenter, 0);
    let second = presenter
        .consume_ready(LIVE, 1, now, 0, 0)
        .expect("the next head");
    assert_eq!(
        presenter.returned.load(Ordering::Relaxed),
        first_charge + second_charge
    );
    drop(first);
    assert_eq!(presenter.returned.load(Ordering::Relaxed), second_charge);
    presenter.clear(now).expect("a clear");
    presenter.connected(super::GENERATION + 1);
    assert_eq!(presenter.residency(), second_charge);
    drop(second);
    assert_eq!(presenter.residency(), 0);
}

#[test]
fn eviction_releases_waiting_positions_but_not_a_returned_owner() {
    let (mut presenter, now) = prepared(10);
    let head = &presenter.scenes[&LIVE].ready[0];
    let bytes = head.step.attribution();
    let done = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect("the head");
    let job = only_job(done.effects.expect("one newly eligible step"));
    assert_eq!(positions(&presenter), WINDOW);
    assert_eq!(presenter.scenes[&LIVE].queue.len(), 1);
    let effects = super::data(
        &mut presenter,
        DataToShell::ScenesRemoved { scenes: vec![LIVE] },
        now,
    )
    .expect("eviction");
    assert!(controls(&effects).is_empty());
    assert!(jobs(effects).is_empty());
    assert!(presenter.scenes.is_empty());
    assert_eq!(presenter.global.usage().objects, 0);
    assert_eq!(presenter.residency(), bytes);
    assert_eq!(presenter.workers(), 1);
    assert!(answer(&mut presenter, job, ms(now, 10)).is_empty());
    assert_eq!(presenter.workers(), 0);
    assert_eq!(presenter.residency(), bytes);
    drop(done.step);
    assert_eq!(presenter.residency(), 0);
}

#[test]
fn a_returned_owner_keeps_the_original_record_charged_after_clear() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let say = "é".repeat(32);
    let source = super::step(&format!(r#"{{"step":{{"id":"narrated","say":"{say}"}}}}"#));
    let job = only_job(commit(&mut presenter, LIVE, 0, source, now).expect("a narration"));
    assert!(answer(&mut presenter, job, now).is_empty());
    let head = &presenter.scenes[&LIVE].ready[0];
    let original = 2 * size_of::<usize>() + size_of::<kumochi_scene::Step>() + 8 + 64;
    let expected = head.full_after + head.step.resident_bytes + original;
    assert_eq!(head.step.attribution(), expected);
    let done = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect("the narrated head");
    assert_eq!(presenter.returned.load(Ordering::Relaxed), expected);
    presenter.clear(now).expect("clear semantic ownership");
    assert_eq!(presenter.residency(), expected);
    drop(done);
    assert_eq!(presenter.residency(), 0);
}

/// Items the overlay paints after sampling every view at `at` ms.
fn sampled_items(presenter: &mut Presenter, at: u64) -> usize {
    let (_, effects) = presenter.sample(at);
    assert!(effects.is_empty());
    let mut items = 0;
    assert!(!presenter.frames.borrow().paint(|frame, _| {
        items += frame.items().len();
        Ok::<_, std::fmt::Error>(())
    }));
    items
}

/// Bounds, ready indices, frame epoch, pending request and returned charge, compared literally.
fn bounds(presenter: &Presenter) -> (u32, u32, Vec<u32>, Option<u64>, Option<u32>, usize) {
    let scene = &presenter.scenes[&LIVE];
    (
        scene.state.view().activated(),
        scene.view.activated(),
        scene.ready.iter().map(|ready| ready.step.index).collect(),
        presenter.frames.borrow().epoch(LIVE),
        scene.pending_consumption,
        presenter.returned.load(Ordering::Relaxed),
    )
}

#[test]
fn a_consumption_installs_its_view_rest_epoch_and_charges_together() {
    let (mut presenter, now) = prepared(2);
    let epoch = presenter.frames.borrow().epoch(LIVE).expect("a frame");
    let owner = presenter.scenes[&LIVE].ready[0].step.attribution();
    assert_eq!(sampled_items(&mut presenter, 0), 0);
    let first = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect("the head");
    assert_eq!(
        bounds(&presenter),
        (1, 1, vec![1], Some(epoch + 1), None, owner)
    );
    let scene = &presenter.scenes[&LIVE];
    assert_eq!(
        (scene.view.layout().revision(), scene.rest.revision()),
        (1, 1)
    );
    assert_eq!(scene.view_bytes, scene.view.resident_bytes());
    assert_eq!(
        presenter.residency(),
        scene.held() + scene.ready[0].step.resident_bytes + owner
    );
    assert_eq!(sampled_items(&mut presenter, 0), 1, "the first step paints");
    drop(first);
    let second = presenter
        .consume_ready(LIVE, 1, now, 0, 0)
        .expect("the next head");
    assert_eq!(presenter.frames.borrow().epoch(LIVE), Some(epoch + 2));
    assert_eq!(sampled_items(&mut presenter, 0), 2, "both steps paint");
    drop(second);
}

#[test]
fn refused_activations_leave_literal_bounds_and_every_charge_unchanged() {
    let (mut presenter, now) = prepared(2);
    let epoch = presenter.frames.borrow().epoch(LIVE);
    let before = shape(&presenter);
    let refused = presenter
        .consume_ready(LIVE, 1, now, 0, 0)
        .expect_err("not the head");
    assert_eq!(refused, ConsumeError::OutOfOrder);
    assert_eq!(bounds(&presenter), (0, 0, vec![0, 1], epoch, None, 0));
    assert_eq!(shape(&presenter), before);
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 2, step(2)), now).expect("an admission"),
    );
    let charged = (semantic(&presenter), presenter.residency());
    for (index, error) in [(0, ConsumeError::Busy), (1, ConsumeError::OutOfOrder)] {
        let refused = presenter
            .consume_ready(LIVE, index, now, 0, 0)
            .expect_err("refused");
        assert_eq!(refused, error);
        assert_eq!(
            bounds(&presenter),
            (0, 0, vec![0, 1], epoch, Some(0), 0),
            "a second index never replaces the pending request"
        );
        assert_eq!((semantic(&presenter), presenter.residency()), charged);
    }
    drop(admission);
}

#[test]
fn a_stale_head_is_refused_before_retirement_with_nothing_changed() {
    for stale_view in [true, false] {
        let (mut presenter, now) = prepared(2);
        let unrelated = kumochi_scene::layout::LayoutState::new(
            &kumochi_scene::SceneState::begin(super::header().begin).expect("the header"),
        );
        let scene = presenter.scenes.get_mut(&LIVE).expect("the scene");
        if stale_view {
            scene.view = kumochi_scene::SceneView::new(&unrelated, 0);
            scene.view_bytes = scene.view.resident_bytes();
        } else {
            scene.rest = unrelated;
            scene.retained = scene.rest.resident_bytes();
        }
        presenter
            .frames
            .borrow_mut()
            .replaced(LIVE, Box::new(Charged(0)));
        let epoch = presenter.frames.borrow().epoch(LIVE);
        let before = shape(&presenter);
        let refused = presenter
            .consume_ready(LIVE, 0, now, 0, 0)
            .expect_err("a stale head");
        assert_eq!(refused, ConsumeError::Stale, "stale view {stale_view}");
        assert_eq!(
            bounds(&presenter),
            (0, 0, vec![0, 1], epoch, None, 0),
            "stale view {stale_view}"
        );
        assert_eq!(shape(&presenter), before, "stale view {stale_view}");
    }
}

#[test]
fn returned_owners_stay_charged_across_view_replacement_and_disconnect() {
    let (mut presenter, now) = prepared(2);
    let ready = &presenter.scenes[&LIVE].ready;
    let charges = [ready[0].step.attribution(), ready[1].step.attribution()];
    let first = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect("the head");
    let second = presenter
        .consume_ready(LIVE, 1, now, 0, 0)
        .expect("the next head replaces the first view");
    let scene = &presenter.scenes[&LIVE];
    assert_eq!(scene.view.activated(), 2);
    assert_eq!(
        presenter.residency(),
        scene.held() + charges[0] + charges[1]
    );
    presenter.disconnected();
    assert!(presenter.scenes.is_empty());
    assert_eq!(presenter.residency(), charges[0] + charges[1]);
    assert_eq!(
        (first.step.step().index, second.step.step().index),
        (0, 1),
        "an owner's metadata outlives its scene"
    );
    drop(first);
    assert_eq!(presenter.residency(), charges[1]);
    drop(second);
    assert_eq!(presenter.residency(), 0);
}

#[test]
fn consumption_forwards_distinct_scene_time_and_narration_segment() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let timed = super::step(
        r#"{"step":{"id":"timed","say":"one two three four","put":[{"shape":{"id":"s","kind":"rect","box":[0,0,10,10]}},{"anim":{"target":"s","track":"x","ease":"linear","at_word":2,"keys":[[0,0],[1000,100]]}}]}}"#,
    );
    let job = only_job(commit(&mut presenter, LIVE, 0, timed, now).expect("step"));
    assert!(answer(&mut presenter, job, now).is_empty());
    let consumed = presenter
        .consume_ready(LIVE, 0, now, 1000, 800)
        .expect("timed head");
    assert!(consumed.effects.expect("no successor").is_empty());
    presenter.sample(1000);
    super::paint_shown(&mut presenter, super::ms(now, 1000));
    for (at, expected) in [(1399, 0.0), (1400, 0.0), (1650, 25.0)] {
        let (_, effects) = presenter.sample(at);
        assert!(effects.is_empty());
        let mut x = None;
        assert!(!presenter.frames.borrow().paint(|frame, _| {
            assert_eq!(frame.items().len(), 1);
            x = Some(frame.items()[0].transform.0[4]);
            Ok::<_, std::fmt::Error>(())
        }));
        assert_eq!(x, Some(expected), "scene time {at}");
    }
}

const PAIR: &str = r#"{"step":{"id":"pair","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10]}},{"shape":{"id":"b","kind":"rect","box":[0,0,10,10]}}]}}"#;
const TRIPLE: &str = r#"{"step":{"id":"triple","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10]}},{"shape":{"id":"b","kind":"rect","box":[0,0,10,10]}},{"shape":{"id":"c","kind":"rect","box":[0,0,10,10]}}]}}"#;
/// Recovered scenes in id order, each larger than the next.
const RECOVERED: [SceneId; 3] = [SceneId(10), SceneId(11), SceneId(12)];

fn queue(values: usize, bytes: usize) -> Pending {
    Pending { values, bytes }
}

/// What `act` sends to the dropper, read behind a gate the dropper has already reached, so
/// everything queued before it is destroyed and counted down first. The meter is process-wide,
/// so only a meter run alone reads exact values.
fn queued<T>(act: impl FnOnce() -> T) -> (Pending, T) {
    struct Gate {
        reached: mpsc::Sender<()>,
        released: mpsc::Receiver<()>,
    }
    impl Drop for Gate {
        fn drop(&mut self) {
            let _reached = self.reached.send(());
            let _released = self.released.recv();
        }
    }
    kumochi_scene::deferred::start().expect("the dropper starts");
    let (reached, at_gate) = mpsc::channel();
    let (open, released) = mpsc::channel();
    drop(Deferred::new(Gate { reached, released }, 0));
    at_gate
        .recv_timeout(Duration::from_secs(5))
        .expect("the dropper reaches the gate");
    assert_eq!(pending(), queue(1, 0), "only the gate: run meters alone");
    let value = act();
    let after = pending();
    drop(open);
    (queue(after.values - 1, after.bytes), value)
}

/// A new empty scene's picture with a [`CACHE`]-byte cache, and its semantic state's charge.
fn empty_scene() -> (usize, usize) {
    let state = SceneState::begin(super::header().begin).expect("the header");
    let rest = LayoutState::new(&state);
    let picture = rest.resident_bytes()
        + SceneView::new(&rest, 0).resident_bytes()
        + Frame::new().resident_bytes()
        + FRAME_CACHE_METADATA_BYTES
        + CACHE;
    (picture, state.attributed_bytes())
}

/// A scene with one step ready, its caches charging `cache` and, from its consumption on,
/// `next`; with the residency that consumption leaves, held owners aside.
fn ready_head(cache: usize, next: usize) -> (Presenter, Instant, usize) {
    // The activated view's own charge, read from an identical scene after a real install.
    let installed = {
        let (mut presenter, now) = prepared(1);
        let done = presenter
            .consume_ready(LIVE, 0, now, 0, 0)
            .expect("an identical head");
        drop(done);
        presenter.scenes[&LIVE].view.resident_bytes()
    };
    let (mut presenter, now) = charged(1, cache);
    presenter.stage = Box::new(Charged(next));
    let scene = &presenter.scenes[&LIVE];
    let head = &scene.ready[0];
    let outgoing = scene.retained + scene.view_bytes + cache + head.step.resident_bytes;
    let incoming = head.full_after + installed + next + head.step.attribution();
    let after = presenter.residency() - outgoing + incoming;
    (presenter, now, after)
}

/// A recovered two-object scene stopped at rest, its caches charging `cache` and, from its kept
/// cut on, `next`; with the picture of the one-object replacement the cut installs.
fn stopped_pair(cache: usize, next: usize) -> (Presenter, Instant, usize) {
    let (_reference, _, job) = recovering(vec![(LIVE, rect("r"))]);
    let reference = job.run(&mut Shaper);
    let Completion::Recovered { scenes, .. } = &reference else {
        panic!("a one-object reference");
    };
    let (state, rest) = scenes[0].1.as_ref().expect("the reference rest");
    let picture = scenes[0].2
        + SceneView::new(rest, state.activated()).resident_bytes()
        + Frame::new().resident_bytes()
        + FRAME_CACHE_METADATA_BYTES
        + next;
    let (mut presenter, now, job) = recovering(vec![(LIVE, super::step(PAIR))]);
    presenter.stage = Box::new(Charged(cache));
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovered rest");
    presenter.stage = Box::new(Charged(next));
    presenter.stop(RUN, now).expect("a stop at rest");
    (presenter, now, picture)
}

/// Three recovered scenes, each larger than the next by id, their caches charging [`CACHE`];
/// with each one's picture, read from the answer before adoption.
fn recovered_three() -> (Presenter, Instant, Completion, [usize; 3]) {
    let steps = [super::step(TRIPLE), super::step(PAIR), rect("r")];
    let (mut presenter, now, job) = recovering(RECOVERED.into_iter().zip(steps).collect());
    presenter.stage = Box::new(Charged(CACHE));
    let done = job.run(&mut Shaper);
    let Completion::Recovered { scenes, .. } = &done else {
        panic!("a recovery answer");
    };
    let pictures = RECOVERED.map(|id| {
        let (_, recovered, retained) = scenes
            .iter()
            .find(|(scene, ..)| *scene == id)
            .expect("every scene recovered");
        let (state, rest) = recovered.as_ref().expect("a recovered rest");
        retained
            + SceneView::new(rest, state.activated()).resident_bytes()
            + Frame::new().resident_bytes()
            + FRAME_CACHE_METADATA_BYTES
            + CACHE
    });
    assert!(
        pictures[0] > pictures[1] && pictures[1] > pictures[2],
        "each scene outweighs the next"
    );
    (presenter, now, done, pictures)
}

#[test]
fn a_new_scene_with_no_held_owner_charges_exactly_its_picture() {
    let (picture, _) = empty_scene();
    let (mut presenter, now) = live();
    presenter.stage = Box::new(Charged(CACHE));
    assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
    assert_eq!(presenter.scenes[&LIVE].cache_bytes, CACHE);
    assert_eq!(presenter.residency(), picture);
}

#[test]
fn a_consumption_with_no_held_owner_charges_exactly_its_own_total() {
    let (mut presenter, now, after) = ready_head(CACHE, NEXT_CACHE);
    let done = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect("no held owner");
    assert_eq!(presenter.scenes[&LIVE].cache_bytes, NEXT_CACHE);
    assert_eq!(presenter.residency(), after);
    drop(done);
}

#[test]
fn a_kept_cut_with_no_held_owner_charges_exactly_its_replacement() {
    let (mut presenter, now, picture) = stopped_pair(CACHE, NEXT_CACHE);
    let revision = presenter.scenes[&LIVE].state.view().revision();
    let cut = super::kept(LIVE).cut;
    let effects = acknowledge(&mut presenter, LIVE, revision, cut, now).expect("the cut");
    assert!(controls(&effects).is_empty());
    assert_eq!(presenter.scenes[&LIVE].cache_bytes, NEXT_CACHE);
    assert_eq!(presenter.residency(), picture);
}

#[test]
fn recovery_evicts_by_scene_id_after_adopting_and_evicted_scenes_never_paint() {
    let clear = |evicted: &[usize]| ControlToBrain::ClearDrawing {
        scene_ids: evicted.iter().map(|&index| RECOVERED[index]).collect(),
    };
    // No held owner keeps all three; at the cap only the middle scene is over, and one byte
    // over the last one is too.
    for (over, kept, clears) in [
        (None, vec![0, 1, 2], vec![]),
        (Some(0), vec![0, 2], vec![clear(&[1])]),
        (Some(1), vec![0], vec![clear(&[1, 2])]),
    ] {
        let (mut presenter, now, done, pictures) = recovered_three();
        let returned = over.map_or(0, |over| {
            MAX_RESIDENT_BYTES - pictures[0] - pictures[2] + over
        });
        presenter.returned.store(returned, Ordering::Relaxed);
        let effects = presenter.completed(done, now).expect("recovery adoption");
        assert_eq!(
            controls(&effects),
            clears.iter().collect::<Vec<_>>(),
            "{over:?} over"
        );
        let ids: Vec<_> = kept.iter().map(|&index| RECOVERED[index]).collect();
        assert_eq!(presenter.scenes.keys().copied().collect::<Vec<_>>(), ids);
        let held: usize = kept.iter().map(|&index| pictures[index]).sum();
        assert_eq!(presenter.residency(), returned + held, "{over:?} over");
        let (_, effects) = presenter.sample(0);
        assert!(effects.is_empty());
        let mut painted = Vec::new();
        assert!(!presenter.frames.borrow().paint(|frame, _| {
            painted.push(Arc::clone(frame.generation().expect("a sampled frame")));
            Ok::<_, std::fmt::Error>(())
        }));
        let views: Vec<_> = ids
            .iter()
            .map(|id| Arc::clone(presenter.scenes[id].view.generation()))
            .collect();
        assert_eq!(painted.len(), views.len(), "only kept scenes paint");
        assert!(
            painted
                .iter()
                .zip(&views)
                .all(|(painted, view)| Arc::ptr_eq(painted, view)),
            "each painted frame samples a kept scene's view, in id order"
        );
        presenter.returned.store(0, Ordering::Relaxed);
        let evicted: Vec<_> = RECOVERED
            .into_iter()
            .filter(|id| !presenter.scenes.contains_key(id))
            .collect();
        for id in evicted {
            assert!(controls(&begin(&mut presenter, id, now)).is_empty());
        }
        assert_eq!(
            presenter.scenes.len(),
            kept.len(),
            "a later valid begin cannot undo eviction"
        );
    }
}

#[test]
#[ignore = "manual isolated queue-meter check; the process-wide dropper must have no other tests"]
fn cache_meter_a_refused_new_scene_queues_one_tuple_with_its_cache_charge() {
    let (picture, semantic) = empty_scene();
    // The frame's own empty wrapper, and for a refusal the new scene's one owner tuple.
    for (over, expected) in [(0, queue(1, 0)), (1, queue(2, semantic + picture))] {
        let (mut presenter, now) = live();
        presenter.stage = Box::new(Charged(CACHE));
        presenter
            .returned
            .store(MAX_RESIDENT_BYTES - picture + over, Ordering::Relaxed);
        let (sent, _) = queued(|| begin(&mut presenter, LIVE, now));
        assert_eq!(sent, expected, "{over} over");
        assert_eq!(presenter.scenes.len(), 1 - over);
    }
}

#[test]
#[ignore = "manual isolated queue-meter check; the process-wide dropper must have no other tests"]
fn cache_meter_a_consumption_queues_its_refused_or_outgoing_cache_once() {
    // Twins whose caches charge nothing queue every other owner the same.
    let [real, free] = [(CACHE, NEXT_CACHE), (0, 0)].map(|(cache, next)| {
        let (mut presenter, now, after) = ready_head(cache, next);
        presenter
            .returned
            .store(MAX_RESIDENT_BYTES - after + 1, Ordering::Relaxed);
        let (refused, result) = queued(|| presenter.consume_ready(LIVE, 0, now, 0, 0));
        assert_eq!(result.err(), Some(ConsumeError::Residency));
        presenter
            .returned
            .store(MAX_RESIDENT_BYTES - after, Ordering::Relaxed);
        let (consumed, result) = queued(|| presenter.consume_ready(LIVE, 0, now, 0, 0));
        drop(result.expect("at the cap"));
        (refused, consumed)
    });
    let [(refused, consumed), (free_refused, free_consumed)] = [real, free];
    assert_eq!(
        free_refused.values, 2,
        "the refused cache and the unused activation"
    );
    assert_eq!(
        refused,
        queue(2, free_refused.bytes + NEXT_CACHE),
        "the refused cache, once"
    );
    assert_eq!(
        consumed,
        queue(free_consumed.values, free_consumed.bytes + CACHE),
        "the outgoing cache, once"
    );
}

#[test]
#[ignore = "manual isolated queue-meter check; the process-wide dropper must have no other tests"]
fn cache_meter_recovery_queues_its_answer_then_four_owners_per_evicted_scene() {
    // Each scene's bound semantic state and held charge, read from a run that keeps all three.
    let owners: [usize; 3] = {
        let (mut presenter, now, done, pictures) = recovered_three();
        presenter.completed(done, now).expect("recovery adoption");
        std::array::from_fn(|index| {
            let scene = &presenter.scenes[&RECOVERED[index]];
            assert_eq!(scene.held(), pictures[index]);
            scene.state.attributed_bytes() + pictures[index]
        })
    };
    for (over, evicted) in [(0, vec![1]), (1, vec![1, 2])] {
        let (mut presenter, now, done, pictures) = recovered_three();
        let Completion::Recovered { scenes, .. } = &done else {
            panic!("a recovery answer");
        };
        let answer = scenes.attribution();
        let returned = MAX_RESIDENT_BYTES - pictures[0] - pictures[2] + over;
        presenter.returned.store(returned, Ordering::Relaxed);
        let (sent, effects) = queued(|| presenter.completed(done, now));
        drop(effects.expect("recovery adoption"));
        let evicted_owners: usize = evicted.iter().map(|&index| owners[index]).sum();
        // Per evicted scene: its two halted queues, its bound state and its held owners.
        assert_eq!(
            sent,
            queue(1 + 4 * evicted.len(), answer + evicted_owners),
            "{over} over"
        );
    }
}

#[test]
#[ignore = "manual isolated queue-meter check; the process-wide dropper must have no other tests"]
fn cache_meter_a_kept_cut_queues_its_old_cache_then_evicts_its_installed_replacement() {
    // Twins whose caches charge nothing queue every other owner the same.
    let [real, free] = [(CACHE, NEXT_CACHE), (0, 0)].map(|(cache, next)| {
        [0, 1].map(|over| {
            let (mut presenter, now, picture) = stopped_pair(cache, next);
            let revision = presenter.scenes[&LIVE].state.view().revision();
            let cut = super::kept(LIVE).cut;
            presenter
                .returned
                .store(MAX_RESIDENT_BYTES - picture + over, Ordering::Relaxed);
            let (sent, effects) = queued(|| acknowledge(&mut presenter, LIVE, revision, cut, now));
            drop(effects.expect("the cut"));
            let owners = presenter.scenes.get(&LIVE).map(|scene| {
                assert_eq!(scene.held(), picture);
                scene.state.attributed_bytes() + picture
            });
            assert_eq!(owners.is_none(), over == 1, "{over} over");
            (sent, owners)
        })
    });
    let [(kept, Some(owners)), (evicted, None)] = real else {
        panic!("the cut fits at the cap and is evicted one byte over");
    };
    let [(free_kept, _), (free_evicted, _)] = free;
    // Two halted queues, the bound state and the installed replacement's owners.
    assert_eq!(evicted, queue(kept.values + 4, kept.bytes + owners));
    assert_eq!(
        kept,
        queue(free_kept.values, free_kept.bytes + CACHE),
        "the old cache, once"
    );
    assert_eq!(evicted.values, free_evicted.values);
    assert_eq!(
        evicted.bytes,
        free_evicted.bytes + CACHE + NEXT_CACHE,
        "the old cache, then the installed one"
    );
}
