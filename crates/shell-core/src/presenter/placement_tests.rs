//! Placement follows the presenter's real install, cut, recovery and resample paths.
use super::{
    GENERATION, LIVE, Presenter, RUN, Shaper, Witness, acknowledge, begin, bound_everywhere,
    commit, kept, live, ms, only_job, recovering, rect, step,
};
use kumochi_scene::{
    Frame,
    geom::{Affine, Rect, Vec2},
};

fn stage() -> Rect {
    Rect {
        origin: Vec2::new(100.0, 200.0),
        size: Vec2::new(400.0, 300.0),
    }
}

fn transform(presenter: &Presenter) -> Affine {
    let mut frame = Frame::new();
    presenter.scenes[&LIVE]
        .view
        .frame(60_000, &mut frame)
        .expect("frame");
    assert_eq!(frame.items().len(), 1, "the fixture keeps its only shape");
    frame.items()[0].transform
}

/// The transform and opacity of each item the shown frames paint.
fn painted(presenter: &Presenter) -> Vec<(Affine, f32)> {
    let mut painted = Vec::new();
    assert!(!presenter.frames().borrow().paint(|frame, _| {
        painted.extend(
            frame
                .items()
                .iter()
                .map(|item| (item.transform, item.opacity)),
        );
        Ok::<_, ()>(())
    }));
    painted
}

#[test]
fn placement_is_installed_for_live_steps_and_kept_after_stop() {
    let (mut presenter, now) = live();
    assert!(presenter.set_stage(stage()));
    begin(&mut presenter, LIVE, now);
    let effects = commit(&mut presenter, LIVE, 0, rect("r"), now).expect("commit");
    presenter
        .completed(only_job(effects).run(&mut Shaper), now)
        .expect("prepared");
    assert!(presenter.placement_due(now));
    presenter.advance(GENERATION, now).expect("activate");
    assert!(!presenter.placement_due(now));
    let expected = Affine::pose(2.0, 0.0, Vec2::new(290.0, 340.0));
    assert_eq!(transform(&presenter), expected);
    presenter.sample(60_000);
    presenter.stop(RUN, now).expect("stop");
    let revision = presenter.scenes[&LIVE].state.view().revision();
    acknowledge(&mut presenter, LIVE, revision, kept(LIVE).cut, now).expect("cut");
    assert_eq!(transform(&presenter), expected);
}

#[test]
fn recovery_uses_the_latest_stage_and_a_stage_change_repaints_without_preparing() {
    let (mut presenter, now, job) = recovering(vec![(LIVE, rect("r"))]);
    presenter.stage = Box::new(Witness);
    assert!(presenter.set_stage(stage()));
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovered");
    assert_eq!(
        transform(&presenter),
        Affine::pose(2.0, 0.0, Vec2::new(290.0, 340.0))
    );
    assert!(presenter.sample(0).0.changed);
    assert!(!presenter.sample(0).0.changed);
    assert!(bound_everywhere(&presenter));
    let primitives = presenter.scenes[&LIVE].rest.items()[0].primitives.to_vec();
    let revision = presenter.scenes[&LIVE].rest.revision();
    assert!(presenter.set_stage(Rect {
        origin: Vec2::new(20.0, 30.0),
        size: Vec2::new(58.0, 58.0)
    }));
    assert_eq!(
        transform(&presenter),
        Affine::pose(1.0, 0.0, Vec2::new(44.0, 54.0))
    );
    assert!(presenter.sample(0).0.changed);
    assert!(!presenter.sample(0).0.changed);
    let frames = presenter.frames();
    assert!(!frames.borrow().paint(|frame, _| {
        let [item] = frame.items() else {
            panic!("placement preserves the one visible item")
        };
        assert_eq!(&*item.primitives, primitives);
        Ok::<_, ()>(())
    }));
    assert!(
        bound_everywhere(&presenter),
        "the new sample still has a matching paint cache"
    );
    let scene = &presenter.scenes[&LIVE];
    assert_eq!(scene.rest.revision(), revision);
    assert_eq!(presenter.workers(), 0);
}

#[test]
fn a_pending_first_paint_is_placed_and_a_stage_change_keeps_its_clock_pending() {
    let (mut presenter, now) = live();
    presenter.set_stage(stage());
    begin(&mut presenter, LIVE, now);
    let source = step(
        r#"{"step":{"id":"fade","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"fade","dur":800}}]}}"#,
    );
    let effects = commit(&mut presenter, LIVE, 0, source, now).expect("commit");
    presenter
        .completed(only_job(effects).run(&mut Shaper), now)
        .expect("prepare");
    presenter.advance(GENERATION, now).expect("activate");
    assert_eq!(
        transform(&presenter),
        Affine::pose(2.0, 0.0, Vec2::new(290.0, 340.0))
    );
    presenter.sample_at(now).expect("a sample");
    presenter.sample_at(ms(now, 2000)).expect("a sample");
    presenter.set_stage(Rect {
        origin: Vec2::new(20.0, 30.0),
        size: Vec2::new(58.0, 58.0),
    });
    assert!(
        presenter
            .sample_at(ms(now, 2000))
            .expect("a sample")
            .0
            .changed
    );
    let frames = presenter.frames();
    frames.borrow_mut().shown = true;
    let placed = Affine::pose(1.0, 0.0, Vec2::new(44.0, 54.0));
    assert_eq!(painted(&presenter), [(placed, 0.0)]);
    presenter.set_stage(stage());
    assert!(
        presenter
            .sample_at(ms(now, 2000))
            .expect("a sample")
            .0
            .changed
    );
    assert!(presenter.begin_painted(ms(now, 2000)));
    presenter.sample_at(ms(now, 2400)).expect("a sample");
    assert_eq!(
        painted(&presenter),
        [(Affine::pose(2.0, 0.0, Vec2::new(290.0, 340.0)), 0.5)]
    );
    assert!(!presenter.begin_painted(ms(now, 2500)));
}

#[test]
fn placement_preserves_an_empty_pending_slot_and_its_late_appearance_clamp() {
    let (mut presenter, now) = live();
    presenter.set_stage(stage());
    begin(&mut presenter, LIVE, now);
    let source = step(
        r#"{"step":{"id":"delayed","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"fade","dur":800,"delay":200}}]}}"#,
    );
    let effects = commit(&mut presenter, LIVE, 0, source, now).expect("commit");
    presenter
        .completed(only_job(effects).run(&mut Shaper), now)
        .expect("prepare");
    presenter.advance(GENERATION, now).expect("activate");
    assert!(!presenter.sample_at(now).expect("a sample").0.visible);
    assert!(
        !presenter
            .sample_at(ms(now, 100))
            .expect("a sample")
            .0
            .visible
    );
    let frames = presenter.frames();
    let epoch = frames.borrow().epoch(LIVE).expect("epoch");
    presenter.set_stage(Rect {
        origin: Vec2::new(20.0, 30.0),
        size: Vec2::new(58.0, 58.0),
    });
    assert_eq!(frames.borrow().epoch(LIVE), Some(epoch));
    assert_eq!(frames.borrow().painted(LIVE), None);
    assert!(!presenter.begin_painted(ms(now, 150)));
    assert!(
        presenter
            .sample_at(ms(now, 300))
            .expect("a sample")
            .0
            .visible
    );
    frames.borrow_mut().shown = true;
    assert_eq!(
        painted(&presenter),
        [(Affine::pose(1.0, 0.0, Vec2::new(44.0, 54.0)), 0.0)]
    );
    let observation = frames.borrow().painted(LIVE).expect("painted");
    presenter.set_stage(stage());
    assert_eq!(frames.borrow().epoch(LIVE), Some(epoch));
    assert_eq!(frames.borrow().painted(LIVE), Some(observation));
    assert!(presenter.begin_painted(ms(now, 400)));
    presenter.sample_at(ms(now, 800)).expect("a sample");
    assert_eq!(
        painted(&presenter),
        [(Affine::pose(2.0, 0.0, Vec2::new(290.0, 340.0)), 0.5)]
    );
    assert!(!presenter.begin_painted(ms(now, 900)));
}
