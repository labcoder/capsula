//! Placement follows the presenter's real install, cut, recovery and resample paths.
use super::{
    GENERATION, LIVE, Presenter, RUN, Shaper, Witness, acknowledge, begin, bound_everywhere,
    commit, controls, data, described, header, kept, live, ms, only_job, painted_rects, presenter,
    recovering, recovery, rect, snapshot, step,
};
use kumochi_protocol::message::{ControlToBrain, DataToShell, SceneId};
use kumochi_scene::{
    Frame, WireId,
    geom::{Affine, Rect, Vec2},
    record::Others,
};

fn stage() -> Rect {
    Rect {
        origin: Vec2::new(100.0, 200.0),
        size: Vec2::new(400.0, 300.0),
    }
}

const WIDE: Rect = Rect {
    origin: Vec2::new(32.0, 177.0),
    size: Vec2::new(2496.0, 1191.0),
};

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

fn painted_pair(presenter: &Presenter) -> [Rect; 2] {
    painted_rects(presenter)
        .try_into()
        .expect("two drawings paint")
}

fn activate_step(
    presenter: &mut Presenter,
    id: SceneId,
    index: u32,
    picture: kumochi_scene::Step,
    now: std::time::Instant,
) {
    let effects = commit(presenter, id, index, picture, now).expect("commit");
    presenter
        .completed(only_job(effects).run(&mut Shaper), now)
        .expect("prepare");
    presenter
        .consume_ready(id, index, now, 0, 0)
        .expect("activate");
}

#[test]
fn a_new_kept_drawing_fits_beside_a_recovered_one_without_moving_it() {
    let (mut presenter, now, job) = recovering(vec![(LIVE, rect("old"))]);
    presenter.set_stage(stage());
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovered");
    presenter.sample(60_000);
    let first = painted_rects(&presenter)[0];

    let next = SceneId(LIVE.0 + 1);
    let mut header = header();
    header.begin.others = Others::Keep;
    header.begin.scene = WireId::try_from("new".to_owned()).expect("scene name");
    data(
        &mut presenter,
        DataToShell::SceneBegin {
            scene: next,
            header,
        },
        now,
    )
    .expect("begin the independent drawing");
    let effects = commit(&mut presenter, next, 0, rect("new"), now).expect("commit");
    presenter
        .completed(only_job(effects).run(&mut Shaper), now)
        .expect("prepare");
    presenter.advance(GENERATION, now).expect("activate");
    presenter.sample(60_000);
    let [kept, new] = painted_pair(&presenter);
    assert_eq!(kept, first);
    assert_eq!(
        new.size, first.size,
        "making room doesn't shrink the new drawing"
    );
    assert!(!new.intersects(first), "both drawings fit without overlap");
    assert_eq!(
        new.union(stage()),
        stage(),
        "the new drawing stays on stage"
    );

    let small = Rect {
        origin: Vec2::new(20.0, 30.0),
        size: Vec2::new(58.0, 58.0),
    };
    presenter.set_stage(small);
    presenter.sample(60_000);
    let centered = Rect::centered(small.center(), Vec2::new(10.0, 10.0));
    assert_eq!(painted_rects(&presenter), [centered, centered]);

    let tall = Rect {
        origin: Vec2::new(-200.0, 400.0),
        size: Vec2::new(160.0, 400.0),
    };
    presenter.set_stage(tall);
    presenter.sample(60_000);
    let [kept, new] = painted_pair(&presenter);
    assert_eq!(kept, Rect::centered(tall.center(), first.size));
    assert_eq!(new.size, first.size);
    assert_eq!(
        new.union(tall),
        tall,
        "the new drawing follows the current stage"
    );
    assert!(
        !new.intersects(kept),
        "a vertical band fits on the new stage"
    );
}

#[test]
fn two_kept_drawings_recover_separately_and_continuing_one_keeps_its_place() {
    let mut presenter = presenter();
    let now = std::time::Instant::now();
    let next = SceneId(LIVE.0 + 1);
    presenter.set_stage(WIDE);
    described(&mut presenter, &[next, LIVE], now);
    let mut effects = Vec::new();
    for (id, name, others) in [(next, "new", Others::Keep), (LIVE, "old", Others::Clear)] {
        let picture = step(
            r#"{"step":{"id":"draw","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,360,260]}}]}}"#,
        );
        let mut frame = snapshot(id, picture);
        let DataToShell::SceneSnapshotV1 { chunk } = &mut frame else {
            panic!("a snapshot chunk");
        };
        let begin = &mut chunk.header.as_mut().expect("first chunk header").begin;
        begin.scene = WireId::try_from(name.to_owned()).expect("scene name");
        begin.others = others;
        begin.size = Some([360.0, 260.0].map(|axis| axis.try_into().expect("canvas dimension")));
        effects.extend(data(&mut presenter, frame, now).expect("a snapshot chunk"));
    }
    presenter
        .completed(recovery(effects).run(&mut Shaper), now)
        .expect("recovered both");
    presenter.sample(60_000);
    let [old, new] = painted_pair(&presenter);
    let size = Vec2::new(720.0, 520.0);
    assert_eq!(old, Rect::centered(WIDE.center(), size));
    assert_eq!(new.size, size);
    assert!(!new.intersects(old));

    let extend = step(
        r#"{"step":{"id":"extend","put":[{"shape":{"id":"extra","kind":"rect","box":[360,0,160,260]}}]}}"#,
    );
    activate_step(&mut presenter, LIVE, 1, extend, now);
    presenter.sample(60_000);
    let [left, right, moved] = painted_rects(&presenter)[..] else {
        panic!("the earlier drawing gained a rectangle");
    };
    let grown = left.union(right);
    assert_eq!(
        grown,
        Rect::centered(WIDE.center(), Vec2::new(1040.0, 520.0))
    );
    assert_eq!(moved.size, size);
    assert!(
        !moved.intersects(grown),
        "a continuation doesn't cover the other drawing"
    );

    let delayed = step(
        r#"{"step":{"id":"delayed","put":[{"shape":{"id":"extra","kind":"rect","box":[0,0,10,10],"delay":800}}]}}"#,
    );
    activate_step(&mut presenter, next, 1, delayed, now);
    presenter.sample(0);
    assert_eq!(painted_rects(&presenter), [left, right, moved]);
    let effects = presenter.stop(RUN, now).expect("stop");
    let cut = controls(&effects)
        .into_iter()
        .find_map(|control| match control {
            ControlToBrain::StopScenes { cuts, .. } => cuts.iter().find(|cut| cut.scene == next),
            _ => None,
        })
        .expect("the newer drawing's cut");
    acknowledge(&mut presenter, next, 2, cut.cut.clone(), now).expect("cut the delayed rectangle");
    presenter.sample(60_000);
    assert_eq!(painted_rects(&presenter), [left, right, moved]);
}

#[test]
fn a_full_width_kept_drawing_fits_at_its_normal_scale_in_a_vertical_band() {
    let (mut presenter, now) = live();
    presenter.set_stage(WIDE);
    for (id, name, others, size, picture) in [
        (
            LIVE,
            "old",
            Others::Clear,
            Some([360.0, 100.0]),
            r#"{"step":{"id":"r","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,360,100]}}]}}"#,
        ),
        (
            SceneId(LIVE.0 + 1),
            "new",
            Others::Keep,
            None,
            r#"{"step":{"id":"r","put":[{"shape":{"id":"r","kind":"rect","box":[500,0,1232,100]}}]}}"#,
        ),
    ] {
        let mut header = header();
        header.begin.scene = WireId::try_from(name.to_owned()).expect("scene name");
        header.begin.others = others;
        header.begin.size = size.map(|size| size.map(|axis| axis.try_into().expect("canvas axis")));
        data(
            &mut presenter,
            DataToShell::SceneBegin { scene: id, header },
            now,
        )
        .expect("begin");
        activate_step(&mut presenter, id, 0, step(picture), now);
    }
    presenter.sample(60_000);
    let [old, new] = painted_pair(&presenter);
    assert!(
        !old.intersects(new),
        "the full-width drawing fits above or below"
    );
    assert!(
        (new.size.x - 2402.4).abs() < 0.01,
        "the new drawing didn't shrink"
    );
    assert_eq!(new.union(WIDE), WIDE);
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
