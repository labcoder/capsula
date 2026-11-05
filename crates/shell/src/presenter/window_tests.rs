//! The read-ahead window, eligibility deadlines and head-only consumption.

use std::{
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use kumochi_protocol::message::{ControlToBrain, DataToShell, RunId, SceneFailure, SceneId};
use kumochi_scene::{DetachedCandidate, deferred::Deferred, limits::MAX_RESIDENT_BYTES};

use super::super::{scene::WINDOW, window::ConsumeError};
use super::{
    Completion, Effect, LIVE, PrepareJob, Presenter, RUN, Shaper, acknowledge, begin, commit,
    commit_as, controls, drained, exhausted, failed, jobs, live, ms, only_job, recovering, rect,
    settle,
};

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
    let (mut presenter, now) = live();
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
        .consume_ready(LIVE, 0, ms(now, 10_000))
        .expect("the head");
    let started = jobs(first.effects.expect("a current scene"));
    assert_eq!(started.len(), 1);
    assert_eq!(positions(&presenter), WINDOW);
    assert_eq!(presenter.deadline(), Some(ms(now, 10_250)));
    let second = presenter
        .consume_ready(LIVE, 1, ms(now, 10_100))
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
            .consume_ready(LIVE, 0, ms(now, 10_000))
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
                .consume_ready(LIVE, consumed, now)
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
            scene.rest.resident_bytes() + scene.view_bytes + scene.frame_bytes,
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
            .consume_ready(scene, index, now)
            .expect_err("refused");
        assert_eq!(refused, error);
        assert_eq!(shape(&presenter), before, "{error:?}");
    }
    drop(presenter.consume_ready(LIVE, 0, now).expect("the head"));
    let after = shape(&presenter);
    let refused = presenter.consume_ready(LIVE, 0, now).expect_err("consumed");
    assert_eq!(refused, ConsumeError::AlreadyConsumed);
    assert_eq!(shape(&presenter), after);
    drop(
        presenter
            .consume_ready(LIVE, 1, now)
            .expect("the last head"),
    );
    let after = shape(&presenter);
    let refused = presenter
        .consume_ready(LIVE, 2, now)
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
    let done = presenter.consume_ready(LIVE, 0, now).expect("the head");
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
    drop(presenter.consume_ready(LIVE, 0, now).expect("the head"));
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
            .consume_ready(LIVE, index, now)
            .expect_err("refused");
        assert_eq!(refused, error, "index {index}");
    }
    let done = presenter
        .consume_ready(LIVE, 20, now)
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
    for over in [0, 1] {
        let (mut presenter, now) = live();
        let rest = kumochi_scene::layout::LayoutState::new(
            &kumochi_scene::SceneState::begin(super::header().begin).expect("the header"),
        );
        let empty = rest.resident_bytes()
            + kumochi_scene::SceneView::new(&rest, 0).resident_bytes()
            + kumochi_scene::Frame::new().resident_bytes()
            + size_of::<std::cell::RefCell<super::super::paint::Frames>>()
            + 2 * size_of::<usize>();
        presenter
            .returned
            .store(MAX_RESIDENT_BYTES - empty + over, Ordering::Relaxed);
        let before = (presenter.global.usage(), presenter.global.generation());
        let effects = begin(&mut presenter, LIVE, now);
        assert_eq!(
            (presenter.global.usage(), presenter.global.generation()),
            before
        );
        if over == 0 {
            assert!(controls(&effects).is_empty());
            assert_eq!(presenter.scenes.len(), 1);
            assert_eq!(presenter.residency(), MAX_RESIDENT_BYTES);
        } else {
            assert_eq!(
                controls(&effects),
                [&ControlToBrain::ClearDrawing {
                    scene_ids: vec![LIVE]
                }]
            );
            assert!(presenter.scenes.is_empty());
            assert_eq!(presenter.residency(), MAX_RESIDENT_BYTES - empty + 1);
        }
        presenter.returned.store(0, Ordering::Relaxed);
        if over == 1 {
            assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
            assert!(presenter.scenes.is_empty(), "a later valid begin cannot undo failure");
            assert_eq!(presenter.residency(), 0);
        }
    }
}

#[test]
fn recovery_checks_its_view_and_frame_at_the_cap() {
    for over in [0, 1] {
        let (mut presenter, now, job) = recovering(vec![(LIVE, rect("r"))]);
        let done = job.run(&mut Shaper);
        let Completion::Recovered { scenes, .. } = &done else {
            panic!("a recovery answer");
        };
        let (state, rest) = scenes[0].1.as_ref().expect("the recovered scene");
        let picture = scenes[0].2
            + kumochi_scene::SceneView::new(rest, state.activated()).resident_bytes()
            + kumochi_scene::Frame::new().resident_bytes()
            + size_of::<std::cell::RefCell<super::super::paint::Frames>>()
            + 2 * size_of::<usize>();
        let returned = MAX_RESIDENT_BYTES - picture + over;
        presenter.returned.store(returned, Ordering::Relaxed);
        let effects = presenter.completed(done, now).expect("recovery adoption");
        if over == 0 {
            assert!(controls(&effects).is_empty());
            assert_eq!(presenter.scenes.len(), 1);
            assert_eq!(presenter.residency(), MAX_RESIDENT_BYTES);
        } else {
            assert_eq!(
                controls(&effects),
                [&ControlToBrain::ClearDrawing {
                    scene_ids: vec![LIVE],
                }]
            );
            assert!(presenter.scenes.is_empty());
            assert_eq!(presenter.residency(), returned);
        }
        presenter.returned.store(0, Ordering::Relaxed);
        if over == 1 {
            assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
            assert!(presenter.scenes.is_empty(), "a later valid begin cannot undo failure");
            assert_eq!(presenter.residency(), 0);
        }
    }
}

#[test]
fn a_kept_cut_checks_its_replacement_view_at_the_cap() {
    for over in [0, 1] {
        let (mut presenter, now, job) = recovering(vec![(LIVE, rect("r"))]);
        presenter
            .completed(job.run(&mut Shaper), now)
            .expect("recovered rest");
        let picture = presenter.residency();
        presenter.stop(RUN, now).expect("a stop at rest");
        let cut = presenter.scenes[&LIVE].cut_now();
        let revision = presenter.scenes[&LIVE].state.view().revision();
        let returned = MAX_RESIDENT_BYTES - picture + over;
        presenter.returned.store(returned, Ordering::Relaxed);
        let effects = acknowledge(&mut presenter, LIVE, revision, cut, now).expect("the cut");
        if over == 0 {
            assert!(controls(&effects).is_empty());
            assert_eq!(presenter.scenes.len(), 1);
            assert_eq!(presenter.residency(), MAX_RESIDENT_BYTES);
        } else {
            assert_eq!(
                controls(&effects),
                [&ControlToBrain::ClearDrawing {
                    scene_ids: vec![LIVE],
                }]
            );
            assert!(presenter.scenes.is_empty());
            assert_eq!(presenter.residency(), returned);
        }
        presenter.returned.store(0, Ordering::Relaxed);
        if over == 1 {
            assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
            assert!(presenter.scenes.is_empty(), "a later valid begin cannot undo failure");
            assert_eq!(presenter.residency(), 0);
        }
    }
}

#[test]
fn a_consumption_fits_at_exactly_the_cap_and_is_refused_one_byte_over() {
    for over in [1, 0] {
        let (mut presenter, now) = prepared(1);
        let scene = &presenter.scenes[&LIVE];
        let head = &scene.ready[0];
        let after = presenter.residency() - scene.retained - head.step.resident_bytes
            + head.full_after
            + head.step.attribution();
        presenter
            .returned
            .store(MAX_RESIDENT_BYTES - after + over, Ordering::Relaxed);
        let stamp = probe(&presenter);
        let before = shape(&presenter);
        let result = presenter.consume_ready(LIVE, 0, now);
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
            drop(done);
        }
        presenter.returned.store(0, Ordering::Relaxed);
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
    let first = presenter.consume_ready(LIVE, 0, now).expect("the head");
    let second_charge = charge(&presenter, 0);
    let second = presenter
        .consume_ready(LIVE, 1, now)
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
    let done = presenter.consume_ready(LIVE, 0, now).expect("the head");
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
        .consume_ready(LIVE, 0, now)
        .expect("the narrated head");
    assert_eq!(presenter.returned.load(Ordering::Relaxed), expected);
    presenter.clear(now).expect("clear semantic ownership");
    assert_eq!(presenter.residency(), expected);
    drop(done);
    assert_eq!(presenter.residency(), 0);
}
