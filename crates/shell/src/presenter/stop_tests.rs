//! Stop on a 3D view: the at-rest fast path and the yaw a Stop latches from its last sample.
#![expect(
    clippy::float_cmp,
    reason = "angles and offsets are exact literals of exactly representable values"
)]

use std::time::Instant;

use kumochi_scene::{Frame, Handle, SceneView, StopCut, text::Bounded};

use super::{LIVE, Presenter, RUN, Shaper, acknowledge, begin, commit, jobs, live, settle, step};

/// A live scene whose one step puts view `v` (with `view` fields), box `s` and `extra`,
/// consumed at scene time 0.
fn activated(view: &str, extra: &str) -> (Presenter, Instant) {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let source = format!(
        r#"{{"step":{{"id":"a","put":[{{"view3d":{{"id":"v"{view}}}}},{{"solid":{{"in":"v","id":"s","shape":"box","size":[1,1,1]}}}}{extra}]}}}}"#
    );
    let mut effects = commit(&mut presenter, LIVE, 0, step(&source), now).expect("a 3D step");
    loop {
        let prepare = jobs(settle(&mut presenter, effects, now).expect("admitted"));
        let Some(job) = prepare.into_iter().next() else {
            break;
        };
        effects = presenter
            .completed(job.run(&mut Shaper), now)
            .expect("prepared");
    }
    drop(
        presenter
            .consume_ready(LIVE, 0, now, 0, 0)
            .expect("the prepared head"),
    );
    (presenter, now)
}
/// Stops the run and applies the brain's cut, which keeps the step's view and box.
fn cut(presenter: &mut Presenter, now: Instant) {
    presenter.stop(RUN, now).expect("a stop");
    let revealed = [0, 1].map(|slot| Handle::new(1, slot).expect("a committed object"));
    let cut = StopCut {
        activated: 1,
        revealed: Bounded::new(revealed.to_vec()).expect("two handles"),
    };
    let revision = presenter.scenes[&LIVE].state.view().revision();
    acknowledge(presenter, LIVE, revision, cut, now).expect("the cut");
}
fn sampled(presenter: &Presenter, now: u64) -> Frame {
    let mut frame = Frame::new();
    presenter.scenes[&LIVE]
        .view
        .frame(now, &mut frame)
        .expect("a fresh frame");
    frame
}
fn yaw(presenter: &Presenter, now: u64) -> f64 {
    sampled(presenter, now).views()[0]
        .camera()
        .expect("a built camera")
        .parameters()[0]
}
fn x(presenter: &Presenter, now: u64) -> f64 {
    sampled(presenter, now).views()[0].poses()[0].apply([0.0; 3])[0]
}
fn view(presenter: &Presenter) -> &SceneView {
    &presenter.scenes[&LIVE].view
}

#[test]
fn a_stop_keeps_the_yaw_its_last_sample_showed_however_late_its_cut_arrives() {
    let (mut presenter, now) = activated(r#","spin":30"#, "");
    presenter.sample(1000);
    presenter.stop(RUN, now).expect("a stop");
    presenter.sample(2000);
    presenter.sample(2500);
    let revealed = [0, 1].map(|slot| Handle::new(1, slot).expect("a committed object"));
    let cut = StopCut {
        activated: 1,
        revealed: Bounded::new(revealed.to_vec()).expect("two handles"),
    };
    let revision = presenter.scenes[&LIVE].state.view().revision();
    acknowledge(&mut presenter, LIVE, revision, cut, now).expect("a cut 1.5 s late");
    // The authored yaw is 30 degrees and the spin adds 30 a second up to the sample at 1000.
    for at in [0, 2500, 20_000] {
        assert_eq!(yaw(&presenter, at), 60.0, "still at {at}");
    }
    assert!(
        view(&presenter).at_rest(0),
        "the carried angle doesn't spin"
    );
}

#[test]
fn a_stop_mid_track_or_before_a_delayed_track_lands_at_the_track_s_end() {
    for (delay, stopped) in [(0, 5000), (5000, 1000)] {
        let track = format!(
            r#",{{"anim":{{"target":"s","track":"pos3","ease":"linear","delay":{delay},"keys":[[0,[0,0,0]],[10000,[10,0,0]]]}}}}"#
        );
        let (mut presenter, now) = activated("", &track);
        presenter.sample(stopped);
        assert!(!view(&presenter).at_rest(stopped));
        cut(&mut presenter, now);
        assert!(
            presenter.sample(stopped).0.changed,
            "the cut replaced the view"
        );
        assert_eq!(
            x(&presenter, stopped),
            10.0,
            "delay {delay}: the end at once"
        );
        assert!(view(&presenter).at_rest(stopped));
    }
}

#[test]
fn only_a_view_at_rest_at_its_last_sample_keeps_its_frame_through_a_kept_cut() {
    let (mut presenter, now) = activated("", "");
    presenter.sample(1000);
    cut(&mut presenter, now);
    assert!(
        !presenter.sample(2000).0.changed,
        "the at-rest view and frame stay"
    );
    let (mut presenter, now) = activated("", "");
    cut(&mut presenter, now);
    assert!(
        presenter.sample(2000).0.changed,
        "with no sample before the Stop, the view takes the cut path"
    );
}
