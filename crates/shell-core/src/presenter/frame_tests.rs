//! The overlay's frames: sampling at rest, the paint observation, and paint failure.

use std::time::Instant;

use kumochi_protocol::message::{ControlToBrain, DataToShell, RunId, SceneDescriptorV1, SceneId};
use kumochi_scene::{SceneView, Step, deferred::Deferred, text::Bounded};

use super::super::{Drawn, Frames, PaintTicket, SceneCache, activation::Shown, paint::Sample};
use super::{
    FRAME, GENERATION, LIVE, Presenter, RUN, Shaper, Witness, acknowledge, begin,
    bound_item_counts, commit_as, controls, data, kept, live, ms, only_job, pair, recovering,
    recovery, rect, settle, snapshot, step,
};

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
    assert_eq!(
        bound_item_counts(&presenter),
        Some(vec![2]),
        "the recovered view"
    );
    presenter.stop(RUN, now).expect("a stop at rest");
    let revision = presenter.scenes[&LIVE].state.view().revision();
    let effects = acknowledge(&mut presenter, LIVE, revision, kept(LIVE).cut, now).expect("cut");
    assert!(controls(&effects).is_empty(), "the kept cut stays");
    assert_eq!(sample(&mut presenter), FIRST, "the replacement view");
    assert_eq!(
        bound_item_counts(&presenter),
        Some(vec![1]),
        "the cut's replacement view"
    );
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
    for live_activation in [false, true] {
        let (mut presenter, now) = if live_activation {
            let (mut presenter, now) = live();
            begin(&mut presenter, LIVE, now);
            let job =
                only_job(commit_as(&mut presenter, RUN, LIVE, 0, rect("r"), now).expect("commit"));
            presenter
                .completed(job.run(&mut Shaper), now)
                .expect("prepared");
            presenter.advance(super::GENERATION, now).expect("activate");
            presenter.sample_at(super::ms(now, 800)).expect("a sample");
            (presenter, now)
        } else {
            let (mut presenter, now) = recovered(vec![(LIVE, rect("r"))]);
            sample(&mut presenter);
            (presenter, now)
        };
        let receipts = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let out = std::rc::Rc::clone(&receipts);
        presenter.observe_draw_timing(move |receipt| out.borrow_mut().push(receipt));
        let frames = presenter.frames();
        frames.borrow_mut().shown = true;
        assert!(!frames.borrow().paint(|_, _| Ok::<_, ()>(())));
        assert!(
            frames.borrow().painted(LIVE).is_some(),
            "the prior paint succeeded"
        );
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
        assert_eq!(presenter.take_shown(super::GENERATION), [None; 4]);
        assert!(receipts.borrow().is_empty());
        let effects = presenter.paint_failed();
        assert_eq!(presenter.stoppable_run(), None);
        assert_eq!(presenter.residency(), 0);
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
}

/// Paints every frame answering `Later`, returning each frame's ticket in paint order.
fn later(frames: &Frames) -> Vec<PaintTicket> {
    let mut tickets = Vec::new();
    assert!(!frames.paint_with(|ticket, _, _| {
        tickets.push(ticket);
        Ok::<_, ()>(Drawn::Later)
    }));
    tickets
}

fn only(tickets: &[PaintTicket]) -> PaintTicket {
    let [ticket] = tickets else {
        panic!("one frame paints");
    };
    *ticket
}

/// A live scene whose first step is activated and not yet sampled.
fn activated(drawing: Step) -> (Presenter, Instant) {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let job = only_job(commit_as(&mut presenter, RUN, LIVE, 0, drawing, now).expect("commit"));
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("prepared");
    presenter.advance(GENERATION, now).expect("activate");
    (presenter, now)
}

const FIRST_SHOWN: [Option<Shown>; 4] = [
    Some(Shown {
        generation: GENERATION,
        run: RUN,
        scene: LIVE,
        index: 0,
    }),
    None,
    None,
    None,
];

#[test]
fn a_later_paint_counts_only_once_presented_and_then_acknowledges_once() {
    let (mut now_path, now) = activated(rect("r"));
    now_path.sample_at(now).expect("a sample");
    now_path.frames.borrow_mut().shown = true;
    assert!(!now_path.frames.borrow().paint(|_, _| Ok::<_, ()>(())));
    let painted_now = now_path.frames.borrow().painted(LIVE);
    assert_eq!(now_path.take_shown(GENERATION), FIRST_SHOWN);

    let (mut presenter, now) = activated(rect("r"));
    presenter.sample_at(now).expect("a sample");
    presenter.frames.borrow_mut().shown = true;
    let ticket = only(&later(&presenter.frames.borrow()));
    assert_eq!((ticket.scene, ticket.hold_until), (LIVE, None));
    assert_eq!(presenter.frames.borrow().painted(LIVE), None);
    assert_eq!(presenter.frames.borrow().observations(), [None; 4]);
    assert_eq!(presenter.take_shown(GENERATION), [None; 4]);
    assert!(!presenter.begin_painted(now), "nothing painted yet");

    assert!(presenter.frames.borrow().presented(ticket));
    let painted = presenter.frames.borrow().painted(LIVE);
    assert_eq!(painted.map(|painted| painted.bound), Some(1));
    assert_eq!(
        painted, painted_now,
        "the same observation a Now paint makes"
    );
    assert_eq!(presenter.take_shown(GENERATION), FIRST_SHOWN);
    assert!(presenter.begin_painted(now));
    assert!(
        presenter.frames.borrow().presented(ticket),
        "a repeated answer"
    );
    assert_eq!(
        presenter.take_shown(GENERATION),
        [None; 4],
        "acknowledged once"
    );
}

#[test]
fn a_late_answer_from_an_older_epoch_changes_nothing() {
    type Event = fn(&mut Presenter, Instant);
    let reconnect = |presenter: &mut Presenter, now: Instant| {
        let generation = GENERATION + 1;
        presenter.connected(generation);
        let membership = Bounded::new(vec![SceneDescriptorV1::new(LIVE, 1, 1, 1).expect("counts")])
            .expect("one scene");
        let effects = presenter
            .described(generation, &membership, now)
            .expect("membership");
        assert!(effects.is_empty());
        let effects = presenter
            .data(
                generation,
                Deferred::new(snapshot(LIVE, pair()), 0),
                FRAME,
                now,
            )
            .expect("the same scene id again");
        let effects = settle(presenter, effects, now).expect("a snapshot");
        presenter
            .completed(recovery(effects).run(&mut Shaper), now)
            .expect("recovered again");
    };
    let replace_view = |presenter: &mut Presenter, now: Instant| {
        presenter.stop(RUN, now).expect("a stop at rest");
        let revision = presenter.scenes[&LIVE].state.view().revision();
        let effects =
            acknowledge(presenter, LIVE, revision, kept(LIVE).cut, now).expect("a replacing cut");
        assert!(controls(&effects).is_empty());
    };
    let hide = |presenter: &mut Presenter, _: Instant| presenter.frames.borrow_mut().hide();
    let cases: [(&str, Event); 3] = [
        ("a hide", hide),
        ("a view replacement", replace_view),
        ("a reconnect", reconnect),
    ];
    for (case, event) in cases {
        let (mut presenter, now) = recovered(vec![(LIVE, pair())]);
        sample(&mut presenter);
        let frames = presenter.frames();
        frames.borrow_mut().shown = true;
        let old = only(&later(&frames.borrow()));
        event(&mut presenter, now);
        sample(&mut presenter);
        frames.borrow_mut().shown = true;
        assert!(!frames.borrow().presented(old), "{case}");
        assert_eq!(frames.borrow().painted(LIVE), None, "{case}");
        assert!(!frames.borrow().mark_failed(old), "{case}");
        assert!(presenter.paint_failed().is_empty(), "{case}");
        assert!(!frames.borrow().paint(|_, _| Ok::<_, ()>(())), "{case}");
        let current = frames
            .borrow()
            .painted(LIVE)
            .expect("the current view still paints");
        assert_eq!(current.bound, 1, "{case}");
    }

    let (mut presenter, now) = recovered(vec![(LIVE, pair())]);
    sample(&mut presenter);
    let frames = presenter.frames();
    frames.borrow_mut().shown = true;
    let old = only(&later(&frames.borrow()));
    presenter.clear(now).expect("a clear");
    assert!(!frames.borrow().presented(old), "a cleared scene");
    assert!(
        !frames.borrow().mark_failed(old),
        "no slot or picture matches"
    );
    assert!(presenter.paint_failed().is_empty());
}

#[test]
fn a_paint_that_could_not_reach_the_screen_never_counts() {
    let (mut presenter, _) = recovered(vec![(LIVE, pair())]);
    sample(&mut presenter);
    let frames = presenter.frames();
    let hidden = only(&later(&frames.borrow()));
    frames.borrow_mut().shown = true;
    assert!(
        !frames.borrow().presented(hidden),
        "made on a hidden overlay"
    );
    assert_eq!(frames.borrow().painted(LIVE), None);
    assert!(frames.borrow().presented(only(&later(&frames.borrow()))));
    assert_eq!(frames.borrow().painted(LIVE).map(|p| p.bound), Some(1));

    let delayed = step(
        r#"{"step":{"id":"delayed","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"none","delay":200}}]}}"#,
    );
    let (mut presenter, now) = activated(delayed);
    assert!(!presenter.sample_at(now).expect("a sample").0.visible);
    presenter.frames.borrow_mut().shown = true;
    let empty = only(&later(&presenter.frames.borrow()));
    assert!(
        presenter
            .sample_at(ms(now, 200))
            .expect("a sample")
            .0
            .visible
    );
    assert!(
        !presenter.frames.borrow().presented(empty),
        "an empty frame"
    );
    assert_eq!(presenter.take_shown(GENERATION), [None; 4]);
    let drawn = only(&later(&presenter.frames.borrow()));
    assert!(presenter.frames.borrow().presented(drawn));
    assert_eq!(presenter.take_shown(GENERATION), FIRST_SHOWN);
}

#[test]
fn a_late_failure_stays_failed_and_clears_its_scene_once() {
    let (mut presenter, now) = activated(rect("r"));
    presenter.sample_at(now).expect("a sample");
    let frames = presenter.frames();
    frames.borrow_mut().shown = true;
    let ticket = only(&later(&frames.borrow()));
    assert!(frames.borrow().mark_failed(ticket));
    assert!(!frames.borrow().mark_failed(ticket), "already marked");
    assert!(!frames.borrow().presented(ticket), "a failure stays failed");
    assert_eq!(frames.borrow().painted(LIVE), None);
    assert_eq!(presenter.take_shown(GENERATION), [None; 4]);
    let mut calls = 0;
    assert!(!frames.borrow().paint(|_, _| {
        calls += 1;
        Ok::<_, ()>(())
    }));
    assert_eq!(calls, 0, "a failed frame paints no more");
    assert_eq!(
        controls(&presenter.paint_failed()),
        [&ControlToBrain::SceneFailed { scene: LIVE }]
    );
    assert!(presenter.scenes.is_empty());
    assert!(!frames.borrow().presented(ticket));
    assert!(!frames.borrow().mark_failed(ticket));
    assert!(presenter.paint_failed().is_empty(), "cleared once");
}

#[test]
fn a_late_answer_for_one_scene_never_lands_on_another_that_shares_its_epoch() {
    let second = SceneId(21);
    let (mut presenter, _) = recovered(vec![(LIVE, rect("r")), (second, pair())]);
    sample(&mut presenter);
    let frames = presenter.frames();
    frames.borrow_mut().hide();
    frames.borrow_mut().shown = true;
    sample(&mut presenter);
    assert_eq!(frames.borrow().epoch(LIVE), frames.borrow().epoch(second));
    let tickets = later(&frames.borrow());
    let [first_ticket, second_ticket] = tickets[..] else {
        panic!("both scenes paint");
    };
    assert_eq!((first_ticket.scene, second_ticket.scene), (LIVE, second));
    assert!(frames.borrow().presented(second_ticket));
    assert_eq!(frames.borrow().painted(LIVE), None);
    assert_eq!(frames.borrow().painted(second).map(|p| p.bound), Some(1));
    assert!(frames.borrow().mark_failed(second_ticket));
    assert_eq!(
        controls(&presenter.paint_failed()),
        [&ControlToBrain::SceneFailed { scene: second }]
    );
    assert!(presenter.scenes.contains_key(&LIVE));
    assert!(!frames.borrow().paint(|_, _| Ok::<_, ()>(())));
    assert_eq!(frames.borrow().painted(LIVE).map(|p| p.bound), Some(1));
}
