use kumochi_protocol::message::{ControlToBrain, DataToShell, SceneFailure, SceneId};
use kumochi_scene::{Step, layout::PrepareError, limits::MAX_GLOBAL_DASH_PIECES};

use super::{
    Completion, LIVE, RUN, Shaper, acknowledge, begin, commit, controls, failed, live, only_job,
    recovering, step,
};

#[test]
fn four_live_scenes_release_a_middle_slot_and_adopt_a_fifth() {
    let (mut presenter, now) = live();
    let state = kumochi_scene::SceneState::begin(super::header().begin).expect("header");
    let layout = kumochi_scene::layout::LayoutState::new(&state);
    let each = layout.resident_bytes()
        + kumochi_scene::SceneView::new(&layout, 0).resident_bytes()
        + kumochi_scene::Frame::new().resident_bytes()
        + size_of::<std::cell::RefCell<super::super::paint::Frames>>()
        + 2 * size_of::<usize>()
        + super::super::Activation::BYTES;
    for id in 1..=4 {
        assert!(controls(&begin(&mut presenter, SceneId(id), now)).is_empty());
    }
    assert_eq!(presenter.residency(), 4 * each);
    for (removed, added, expected) in [
        (
            2,
            5,
            [
                Some(SceneId(1)),
                Some(SceneId(3)),
                Some(SceneId(4)),
                Some(SceneId(5)),
            ],
        ),
        (
            3,
            6,
            [
                Some(SceneId(1)),
                Some(SceneId(4)),
                Some(SceneId(5)),
                Some(SceneId(6)),
            ],
        ),
    ] {
        super::data(
            &mut presenter,
            DataToShell::ScenesRemoved {
                scenes: vec![SceneId(removed)],
            },
            now,
        )
        .expect("middle removal");
        assert_eq!(presenter.residency(), 3 * each);
        assert!(controls(&begin(&mut presenter, SceneId(added), now)).is_empty());
        assert_eq!(presenter.scenes.len(), 4);
        assert_eq!(presenter.residency(), 4 * each);
        assert_eq!(presenter.frames.borrow().ids(), expected);
    }
    presenter.disconnected();
    assert_eq!(presenter.residency(), 0);
    assert_eq!(presenter.frames.borrow().ids(), [None; 4]);
}

fn anchored(count: usize) -> Step {
    let mut operations =
        vec![r#"{"shape":{"id":"anchor","kind":"rect","box":[-0.5,-0.5,1,1]}}"#.to_owned()];
    let mut left = count;
    let mut index = 0;
    while left > 0 {
        let mut pieces = left.min(1902);
        if left - pieces == 1 {
            pieces -= 1;
        }
        assert!(pieces >= 2);
        let length = (pieces - 2) * 10;
        operations.push(format!(r#"{{"line":{{"id":"wire{index}","from":{{"ref":"anchor"}},"to":[{length},0],"dash":true}}}}"#));
        index += 1;
        left -= pieces;
    }
    step(&format!(
        r#"{{"step":{{"id":"anchored","put":[{}]}}}}"#,
        operations.join(",")
    ))
}

#[test]
fn anchored_prepared_work_fits_at_the_limit_and_fails_one_over() {
    let limit = MAX_GLOBAL_DASH_PIECES;
    for over in [0, 1] {
        let (mut presenter, now) = live();
        begin(&mut presenter, LIVE, now);
        let job = only_job(
            commit(&mut presenter, LIVE, 0, anchored(limit + over), now)
                .expect("shared admission accepts refs"),
        );
        assert_eq!(presenter.global.usage().dash_pieces, 0);
        let done = job.run(&mut Shaper);
        let Completion::Prepared { result, .. } = &done else {
            panic!("a prepared answer");
        };
        if over == 0 {
            let prepared = result.as_ref().expect("valid anchored geometry");
            assert_eq!(prepared.after.paint_work().dash_pieces, limit);
        } else {
            assert!(
                matches!(&**result, Err(PrepareError::OverResidency)),
                "the worker refuses work over the D1 cap"
            );
        }
        let effects = presenter.completed(done, now).expect("current answer");
        if over == 0 {
            assert!(controls(&effects).is_empty());
            assert_eq!(presenter.scenes[&LIVE].ready.len(), 1);
            assert_eq!(presenter.paint_work().dash_pieces, limit);
        } else {
            assert_eq!(
                controls(&effects),
                [&failed(LIVE, 0, SceneFailure::Residency)]
            );
            assert_eq!(presenter.scenes[&LIVE].ready.len(), 0);
            assert_eq!(presenter.paint_work().dash_pieces, 0);
            let later = commit(&mut presenter, LIVE, 1, super::rect("later"), now)
                .expect("late valid commit");
            assert!(super::jobs(later).is_empty());
            assert_eq!(
                presenter.scenes[&LIVE].ready.len(),
                0,
                "later valid input cannot undo failure"
            );
        }
    }
}

#[test]
fn anchored_recovery_checks_exact_work_and_remembers_its_refusal() {
    let limit = MAX_GLOBAL_DASH_PIECES;
    for over in [0, 1] {
        let (mut presenter, now, job) = recovering(vec![(LIVE, anchored(limit + over))]);
        let effects = presenter
            .completed(job.run(&mut Shaper), now)
            .expect("recovery answer");
        if over == 0 {
            assert!(controls(&effects).is_empty());
            assert_eq!(presenter.paint_work().dash_pieces, limit);
        } else {
            assert_eq!(
                controls(&effects),
                [&ControlToBrain::ClearDrawing {
                    scene_ids: vec![LIVE]
                }]
            );
            assert_eq!(presenter.paint_work().dash_pieces, 0);
            assert!(controls(&begin(&mut presenter, LIVE, now)).is_empty());
            assert!(
                presenter.scenes.is_empty(),
                "a later valid begin cannot undo failure"
            );
        }
    }
}

#[test]
fn recovery_sums_work_across_scenes_before_accepting_the_next() {
    let limit = MAX_GLOBAL_DASH_PIECES;
    let first = limit / 2;
    let other = SceneId(LIVE.0 + 1);
    for over in [0, 1] {
        let (mut presenter, now, job) = recovering(vec![
            (LIVE, anchored(first)),
            (other, anchored(limit - first + over)),
        ]);
        let effects = presenter
            .completed(job.run(&mut Shaper), now)
            .expect("two recovered scenes");
        if over == 0 {
            assert!(controls(&effects).is_empty());
            assert_eq!(presenter.scenes.len(), 2);
            assert_eq!(presenter.paint_work().dash_pieces, limit);
        } else {
            assert_eq!(
                controls(&effects),
                [&ControlToBrain::ClearDrawing {
                    scene_ids: vec![other]
                }]
            );
            assert_eq!(presenter.scenes.len(), 1);
            assert_eq!(presenter.paint_work().dash_pieces, first);
        }
    }
}

#[test]
fn kept_cut_replaces_the_cached_paint_work() {
    let (mut presenter, now, job) = recovering(vec![(LIVE, anchored(16))]);
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovered line");
    assert_eq!(presenter.paint_work().dash_pieces, 16);
    presenter.stop(RUN, now).expect("stop at rest");
    let revision = presenter.scenes[&LIVE].state.view().revision();
    let effects = acknowledge(&mut presenter, LIVE, revision, super::kept(LIVE).cut, now)
        .expect("keep only anchor");
    assert!(controls(&effects).is_empty());
    assert_eq!(presenter.scenes.len(), 1);
    assert_eq!(presenter.paint_work().dash_pieces, 0);
}

#[test]
fn consuming_a_removal_releases_the_previous_rest_work() {
    let (mut presenter, now, job) = recovering(vec![(LIVE, anchored(16))]);
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("recovered line");
    let removal = step(r#"{"step":{"id":"remove","remove":["wire0"]}}"#);
    let job = only_job(commit(&mut presenter, LIVE, 1, removal, now).expect("remove line"));
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("prepared removal");
    assert_eq!(
        presenter.paint_work().dash_pieces,
        16,
        "the old rest can still paint"
    );
    let consumed = presenter
        .consume_ready(LIVE, 1, now, 0, 0)
        .expect("consume removal");
    consumed.effects.expect("remaining work resumes");
    assert_eq!(presenter.paint_work().dash_pieces, 0);
}

#[test]
fn ready_states_reserve_componentwise_maxima_until_the_old_rest_is_replaced() {
    use kumochi_scene::display::PaintWork;

    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = step(
        r#"{"step":{"id":"dash","put":[{"path":{"id":"wire","d":"M0 0 L100 0","dash":true}}]}}"#,
    );
    let job = only_job(commit(&mut presenter, LIVE, 0, first, now).expect("dashed step"));
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("first answer");
    assert_eq!(
        presenter.paint_work(),
        PaintWork {
            dash_pieces: 12,
            ..PaintWork::default()
        }
    );

    let second = step(
        r#"{"step":{"id":"rect","remove":["wire"],"put":[{"shape":{"id":"box","kind":"rect","box":[0,0,100,60],"radius":10,"fill":"blue"}}]}}"#,
    );
    let job = only_job(commit(&mut presenter, LIVE, 1, second, now).expect("replacement step"));
    presenter
        .completed(job.run(&mut Shaper), now)
        .expect("second answer");
    let both = PaintWork {
        dash_pieces: 12,
        fill_vertices: 25,
        largest_fill: 25,
        outline_points: 25,
        largest_outline: 25,
    };
    assert_eq!(presenter.paint_work(), both);
    let consumed = presenter
        .consume_ready(LIVE, 0, now, 0, 0)
        .expect("first consumption");
    consumed.effects.expect("resume after first");
    assert_eq!(presenter.paint_work(), both, "the dashed rest still paints");
    let consumed = presenter
        .consume_ready(LIVE, 1, now, 0, 0)
        .expect("second consumption");
    consumed.effects.expect("resume after second");
    assert_eq!(
        presenter.paint_work(),
        PaintWork {
            dash_pieces: 0,
            ..both
        }
    );
}
