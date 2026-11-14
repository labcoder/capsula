//! The overlay's frames: sampling at rest, the paint observation, and paint failure.

use std::{sync::Arc, time::Instant};

use kumochi_protocol::message::{ControlToBrain, DataToShell, RunId, SceneId};
use kumochi_scene::{SceneView, Step};

use super::super::{SceneCache, paint::Sample};
use super::{
    LIVE, Presenter, Probed, RUN, Shaper, Witness, acknowledge, begin, bound_everywhere, commit_as,
    controls, data, drained, hold_dropper, kept, live, only_job, recovering, rect, step,
};

fn pair() -> Step {
    step(
        r#"{"step":{"id":"pair","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10]}},{"shape":{"id":"b","kind":"rect","box":[0,0,10,10]}}]}}"#,
    )
}

fn recovered(steps: Vec<(SceneId, Step)>) -> (Presenter, Instant) {
    let (mut presenter, now, job) = recovering(steps);
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovered rests");
    (presenter, now)
}

fn sample(presenter: &mut Presenter) -> Sample {
    let (sample, effects) = presenter.sample(0);
    assert!(effects.is_empty(), "every frame samples");
    sample
}

const FIRST: Sample = Sample {
    visible: true,
    changed: true,
};

const REST: Sample = Sample {
    visible: true,
    changed: false,
};

#[test]
fn a_resting_drawing_changes_once_and_never_asks_to_repaint_again() {
    let (mut presenter, _) = recovered(vec![(LIVE, rect("r"))]);
    assert_eq!(sample(&mut presenter), FIRST);
    assert_eq!(sample(&mut presenter), REST);
    let (later, effects) = presenter.sample(60_000);
    assert!(effects.is_empty(), "a later instant still samples");
    assert_eq!(later, REST);
}

#[test]
fn an_empty_scene_is_not_visible_and_its_removal_changes_the_sample_once() {
    let (mut presenter, now) = live();
    assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
    let empty = Sample {
        visible: false,
        changed: true,
    };
    assert_eq!(sample(&mut presenter), empty);
    data(
        &mut presenter,
        DataToShell::ScenesRemoved { scenes: vec![LIVE] },
        now,
    )
    .expect("a removal");
    assert_eq!(sample(&mut presenter), empty);
    assert_eq!(
        sample(&mut presenter),
        Sample {
            visible: false,
            changed: false,
        }
    );
}

#[test]
fn frames_paint_in_scene_order_whatever_order_they_recovered_in() {
    let (mut presenter, _) = recovered(vec![(SceneId(30), pair()), (SceneId(10), rect("r"))]);
    assert_eq!(sample(&mut presenter), FIRST);
    let mut items = Vec::new();
    let failed = presenter.frames().borrow().paint(|frame, _| {
        items.push(frame.paint_items().count());
        Ok::<_, std::fmt::Error>(())
    });
    assert!(!failed);
    assert_eq!(items, [1, 2]);
}

#[test]
fn only_a_shown_paint_is_observed_once_per_epoch_and_bound() {
    let (mut presenter, now) = recovered(vec![(LIVE, pair())]);
    sample(&mut presenter);
    let frames = presenter.frames();
    let ok = |_: &kumochi_scene::Frame, _: &mut dyn SceneCache| Ok::<_, std::fmt::Error>(());
    assert!(!frames.borrow().paint(ok));
    assert_eq!(frames.borrow().painted(LIVE), None, "a hidden overlay");
    assert!(!presenter.drawn());
    frames.borrow_mut().shown = true;
    assert!(presenter.drawn());
    assert!(!frames.borrow().paint(ok));
    let seen = frames.borrow().painted(LIVE).expect("a shown paint");
    assert_eq!(seen.bound, 1);
    assert!(frames.borrow_mut().acknowledge(LIVE, seen));
    assert!(!frames.borrow_mut().acknowledge(LIVE, seen), "deduplicated");
    assert!(!frames.borrow().paint(ok));
    assert!(
        !frames.borrow_mut().acknowledge(LIVE, seen),
        "a repaint of the same bound"
    );

    presenter.stop(RUN, now).expect("a stop at rest");
    let revision = presenter.scenes[&LIVE].state.view().revision();
    let effects = acknowledge(&mut presenter, LIVE, revision, kept(LIVE).cut, now).expect("cut");
    assert!(controls(&effects).is_empty(), "the kept cut stays");
    assert_eq!(frames.borrow().painted(LIVE), None, "the old view's paint");
    assert!(
        !frames.borrow_mut().acknowledge(LIVE, seen),
        "a stale epoch"
    );
    assert_eq!(sample(&mut presenter), FIRST);
    assert!(!frames.borrow().paint(ok));
    let cut = frames.borrow().painted(LIVE).expect("the cut rest's paint");
    assert_ne!(cut.epoch, seen.epoch);
    assert_eq!(cut.bound, seen.bound);
    assert!(!frames.borrow_mut().acknowledge(LIVE, seen), "still stale");
    assert!(frames.borrow_mut().acknowledge(LIVE, cut));
}

#[test]
fn recovery_and_a_replacing_cut_bind_caches_staged_from_their_new_views() {
    let (mut presenter, now, job) = recovering(vec![(LIVE, pair())]);
    presenter.stage = Box::new(Witness);
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovered rests");
    assert_eq!(sample(&mut presenter), FIRST);
    assert!(bound_everywhere(&presenter), "the recovered view");
    presenter.stop(RUN, now).expect("a stop at rest");
    let revision = presenter.scenes[&LIVE].state.view().revision();
    let effects = acknowledge(&mut presenter, LIVE, revision, kept(LIVE).cut, now).expect("cut");
    assert!(controls(&effects).is_empty(), "the kept cut stays");
    assert_eq!(sample(&mut presenter), FIRST, "the replacement view");
    assert!(bound_everywhere(&presenter), "the cut's replacement view");
}

#[test]
fn a_replacing_cut_sends_the_old_cache_to_the_dropper_with_its_rest_and_view() {
    let probe = Arc::new(());
    let (mut presenter, now, job) = recovering(vec![(LIVE, pair())]);
    presenter.stage = Box::new(Probed(Arc::clone(&probe)));
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovered rests");
    assert_eq!(Arc::strong_count(&probe), 3, "probe, stage, cache");
    presenter.stop(RUN, now).expect("a stop at rest");
    let revision = presenter.scenes[&LIVE].state.view().revision();
    let release = hold_dropper();
    let effects = acknowledge(&mut presenter, LIVE, revision, kept(LIVE).cut, now).expect("cut");
    assert!(controls(&effects).is_empty(), "the kept cut stays");
    assert_eq!(Arc::strong_count(&probe), 4, "the new and the queued old");
    drop(release);
    assert!(drained(&probe, 3));
}

#[test]
fn a_cut_that_changes_nothing_keeps_the_rest_view_and_paint_observation() {
    let (mut presenter, now) = recovered(vec![(LIVE, rect("r"))]);
    assert_eq!(sample(&mut presenter), FIRST);
    let frames = presenter.frames();
    frames.borrow_mut().shown = true;
    assert!(!frames.borrow().paint(|_, _| Ok::<_, std::fmt::Error>(())));
    let seen = frames.borrow().painted(LIVE).expect("a shown paint");
    let cut = presenter.scenes[&LIVE].cut_now();
    presenter.stop(RUN, now).expect("a stop at rest");
    let effects = acknowledge(&mut presenter, LIVE, 1, cut, now).expect("the cut");
    assert!(controls(&effects).is_empty(), "the kept cut stays");
    assert_eq!(
        frames.borrow().painted(LIVE),
        Some(seen),
        "the same view's paint"
    );
    assert_eq!(sample(&mut presenter), REST);
    assert!(frames.borrow_mut().acknowledge(LIVE, seen));
    let job = only_job(
        commit_as(&mut presenter, RunId(6), LIVE, 1, rect("s"), now).expect("a step after the cut"),
    );
    let effects = presenter
        .completed(job.run(&mut Shaper), now)
        .expect("prepared on the kept rest");
    assert!(controls(&effects).is_empty());
    assert_eq!(presenter.scenes[&LIVE].ready.len(), 1);
}

#[test]
fn hiding_forgets_every_old_host_paint_without_discarding_sampled_geometry() {
    let second = SceneId(21);
    let (mut presenter, _) = recovered(vec![(LIVE, rect("r")), (second, pair())]);
    assert_eq!(sample(&mut presenter), FIRST);
    let frames = presenter.frames();
    let paint = |_: &kumochi_scene::Frame, _: &mut dyn SceneCache| Ok::<_, std::fmt::Error>(());
    frames.borrow_mut().shown = true;
    assert!(!frames.borrow().paint(paint));
    let seen = [LIVE, second].map(|scene| frames.borrow().painted(scene).expect("shown paint"));
    assert_eq!(seen.map(|paint| paint.bound), [1, 1]);
    assert!(frames.borrow_mut().acknowledge(LIVE, seen[0]));
    frames.borrow_mut().hide();
    assert!(!presenter.drawn());
    for (scene, old) in [LIVE, second].into_iter().zip(seen) {
        assert_eq!(frames.borrow().painted(scene), None);
        assert!(!frames.borrow_mut().acknowledge(scene, old));
    }
    let mut counts = Vec::new();
    assert!(!frames.borrow().paint(|frame, _| {
        counts.push(frame.items().len());
        Ok::<_, std::fmt::Error>(())
    }));
    assert_eq!(counts, [1, 2]);
    assert_eq!(sample(&mut presenter), REST);
    frames.borrow_mut().shown = true;
    assert!(!frames.borrow().paint(paint));
    for (scene, old) in [LIVE, second].into_iter().zip(seen) {
        let new = frames.borrow().painted(scene).expect("fresh host paint");
        assert_eq!(new.bound, 1);
        assert_eq!(new.epoch, seen[0].epoch.max(seen[1].epoch) + 1);
        assert!(!frames.borrow_mut().acknowledge(scene, old));
        assert!(frames.borrow_mut().acknowledge(scene, new));
        assert!(!frames.borrow_mut().acknowledge(scene, new));
    }
}

#[test]
fn a_frame_its_view_refuses_clears_its_scene_and_stays_cleared() {
    let (mut presenter, now) = recovered(vec![(LIVE, rect("r"))]);
    assert_eq!(sample(&mut presenter), FIRST);
    let scene = presenter.scenes.get_mut(&LIVE).expect("the scene");
    let replacement = SceneView::new(&scene.rest, scene.state.view().activated());
    let unannounced = std::mem::replace(&mut scene.view, replacement);
    let (sampled, effects) = presenter.sample(0);
    drop(unannounced);
    assert_eq!(
        controls(&effects),
        [&ControlToBrain::SceneFailed { scene: LIVE }]
    );
    assert_eq!(
        sampled,
        Sample {
            visible: false,
            changed: true,
        }
    );
    assert!(presenter.scenes.is_empty());
    assert_eq!(presenter.frames.borrow().ids(), [None; 4]);
    assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
    assert!(presenter.scenes.is_empty(), "a later begin can't undo it");
}

#[test]
fn a_frame_that_fails_to_paint_paints_no_more_and_clears_its_scene_once() {
    let (mut presenter, now) = recovered(vec![(LIVE, rect("r"))]);
    sample(&mut presenter);
    let frames = presenter.frames();
    frames.borrow_mut().shown = true;
    let mut calls = 0;
    assert!(frames.borrow().paint(|_, _| {
        calls += 1;
        Err(std::fmt::Error)
    }));
    assert!(!frames.borrow().paint(|_, _| {
        calls += 1;
        Ok::<_, std::fmt::Error>(())
    }));
    assert_eq!(calls, 1, "a failed frame paints no more");
    assert_eq!(frames.borrow().painted(LIVE), None);
    let effects = presenter.paint_failed();
    assert_eq!(
        controls(&effects),
        [&ControlToBrain::SceneFailed { scene: LIVE }]
    );
    assert!(presenter.scenes.is_empty());
    assert_eq!(presenter.frames.borrow().ids(), [None; 4]);
    assert!(presenter.paint_failed().is_empty(), "cleared once");
    assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
    assert!(presenter.scenes.is_empty(), "a later begin can't undo it");
}
