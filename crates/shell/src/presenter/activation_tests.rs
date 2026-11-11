use std::{sync::atomic::Ordering, time::Instant};

use kumochi_protocol::message::{ControlToBrain, RunId, SceneFailure, SceneId};
use kumochi_scene::{Step, limits::MAX_RESIDENT_BYTES, record::Start};

use super::super::{activation::Shown, scene::Cut};
use super::{
    GENERATION, LIVE, Presenter, RUN, Shaper, acknowledge, arrive, begin, commit_as, commit_frame,
    controls, jobs, live, ms, only_admission, only_job, paint_shown, recovering, rect, step,
};

fn prepared(steps: &[(RunId, Step)]) -> (Presenter, Instant) {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    for (index, (run, step)) in steps.iter().enumerate() {
        let effects = commit_as(
            &mut presenter,
            *run,
            LIVE,
            u32::try_from(index).expect("index"),
            step.clone(),
            now,
        )
        .expect("commit");
        let job = only_job(effects);
        assert!(
            presenter
                .completed(job.run(&mut Shaper), now)
                .expect("prepare")
                .is_empty()
        );
    }
    (presenter, now)
}

fn bound(presenter: &Presenter) -> u32 {
    presenter.scenes[&LIVE].view.activated()
}

#[test]
fn one_absolute_wake_keeps_its_identity_across_fractional_millisecond_samples() {
    use std::time::Duration;
    let first = step(
        r#"{"step":{"id":"first","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"none"}}]}}"#,
    );
    let later = step(r#"{"step":{"id":"later","start":{"after":"first","ms":2200}}}"#);
    let (mut presenter, now) = prepared(&[(RUN, first), (RUN, later)]);
    presenter
        .advance(GENERATION, now)
        .expect("first activation");
    assert_eq!(bound(&presenter), 1);
    for ns in [0, 1, 999_999, 1_234_567, 2_999_999_999] {
        assert_eq!(
            presenter.next_wake(now + Duration::from_nanos(ns)),
            Some(ms(now, 3000))
        );
    }
    let delayed = step(
        r#"{"step":{"id":"delayed","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10],"enter":"none","delay":200}}]}}"#,
    );
    let (mut presenter, now) = prepared(&[(RUN, delayed)]);
    presenter
        .advance(GENERATION, now)
        .expect("delayed activation");
    for ns in [0, 1, 999_999, 199_999_999] {
        assert_eq!(
            presenter.next_wake(now + Duration::from_nanos(ns)),
            Some(ms(now, 200))
        );
    }
    assert!(presenter.live_work(now));
    assert!(!presenter.animating(now));
}

fn paint(presenter: &Presenter) {
    assert!(!presenter.frames.borrow().paint(|_, _| Ok::<_, ()>(())));
}

fn shown(run: RunId, index: u32) -> [Option<Shown>; 4] {
    [
        Some(Shown {
            generation: GENERATION,
            run,
            scene: LIVE,
            index,
        }),
        None,
        None,
        None,
    ]
}

#[test]
fn synthetic_end_offsets_late_readiness_and_runs_use_one_scene_origin() {
    let words = std::iter::repeat_n("w", 200).collect::<Vec<_>>().join(" ");
    let first = step(&format!(r#"{{"step":{{"id":"first","say":"{words}"}}}}"#));
    let second = rect("second");
    let third = step(r#"{"step":{"id":"third","start":{"after":"first","ms":200}}}"#);
    let (mut presenter, now) = prepared(&[(RUN, first), (RunId(6), second), (RunId(7), third)]);
    for (at, expected) in [(100, 1), (72_827, 1), (72_828, 2), (73_027, 2), (73_028, 3)] {
        assert!(
            presenter
                .advance(GENERATION, ms(now, at))
                .expect("advance")
                .is_empty()
        );
        assert_eq!(bound(&presenter), expected, "at {at}");
    }
    assert_eq!(presenter.stoppable_run(), Some(RunId(7)));
    assert_eq!(presenter.returned.load(Ordering::Relaxed), 0);
    assert_eq!(
        presenter.scenes[&LIVE].activation.time(ms(now, 73_100)),
        73_100
    );
}

#[test]
fn now_supersedes_without_fabricating_hidden_or_empty_acknowledgements() {
    let empty = step(r#"{"step":{"id":"empty","start":"now","remove":["r"]}}"#);
    let mut last = rect("last");
    last.start = Start::Now;
    let (mut presenter, now) = prepared(&[(RUN, rect("r")), (RUN, empty), (RunId(6), last)]);
    presenter.advance(GENERATION, now).expect("first");
    presenter.advance(GENERATION, now).expect("empty");
    assert_eq!(bound(&presenter), 2);
    assert!(!presenter.sample_at(now).0.visible);
    presenter.frames.borrow_mut().shown = true;
    paint(&presenter);
    assert_eq!(presenter.take_shown(GENERATION), [None; 4]);
    presenter.advance(GENERATION, now).expect("last");
    presenter.sample_at(ms(now, 800));
    paint(&presenter);
    assert_eq!(presenter.take_shown(GENERATION), shown(RunId(6), 2));
    assert_eq!(presenter.take_shown(GENERATION), [None; 4]);
    assert_eq!(
        presenter.stoppable_run(),
        Some(RunId(6)),
        "ack retains the Stop token"
    );
}

#[test]
fn visibility_epochs_rebind_fresh_paints_without_resetting_activation_once_state() {
    let delayed = step(
        r#"{"step":{"id":"delayed","start":"now","remove":["r"],"put":[{"shape":{"id":"d","kind":"rect","box":[0,0,10,10],"delay":200,"enter":"none"}}]}}"#,
    );
    let (mut presenter, now) = prepared(&[(RUN, rect("r")), (RunId(6), delayed)]);
    presenter.advance(GENERATION, now).expect("first");
    presenter.sample_at(now);
    presenter.frames.borrow_mut().shown = true;
    paint(&presenter);
    let old = presenter
        .frames
        .borrow()
        .painted(LIVE)
        .expect("first paint");
    presenter.advance(GENERATION, now).expect("delayed");
    assert!(!presenter.sample_at(now).0.visible);
    presenter.frames.borrow_mut().hide();
    assert!(!presenter.frames.borrow_mut().acknowledge(LIVE, old));
    assert_eq!(presenter.take_shown(GENERATION), [None; 4]);
    assert!(presenter.sample_at(ms(now, 200)).0.visible);
    presenter.frames.borrow_mut().shown = true;
    assert_eq!(
        presenter.take_shown(GENERATION),
        [None; 4],
        "a fresh paint is required"
    );
    paint(&presenter);
    assert_eq!(presenter.take_shown(GENERATION + 1), [None; 4]);
    assert_eq!(presenter.take_shown(GENERATION), shown(RunId(6), 1));
    presenter.frames.borrow_mut().hide();
    presenter.sample_at(ms(now, 400));
    presenter.frames.borrow_mut().shown = true;
    paint(&presenter);
    assert_eq!(
        presenter.take_shown(GENERATION),
        [None; 4],
        "same activation, new epoch"
    );
}

#[test]
fn a_failed_epoch_cannot_deliver_an_earlier_success() {
    let (mut presenter, now) = prepared(&[(RUN, rect("r"))]);
    presenter.advance(GENERATION, now).expect("activate");
    presenter.sample_at(ms(now, 800));
    presenter.frames.borrow_mut().shown = true;
    paint(&presenter);
    assert!(presenter.frames.borrow().paint(|_, _| Err::<(), _>(())));
    assert_eq!(presenter.take_shown(GENERATION), [None; 4]);
    assert_eq!(
        controls(&presenter.paint_failed()),
        [&ControlToBrain::ClearDrawing {
            scene_ids: vec![LIVE],
        }]
    );
    assert_eq!(presenter.stoppable_run(), None);
    assert_eq!(presenter.residency(), 0);
}

#[test]
fn busy_retains_the_head_until_its_real_notification() {
    let (mut presenter, now) = prepared(&[(RUN, rect("r"))]);
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 1, rect("next")), now).expect("lane"),
    );
    assert!(presenter.advance(GENERATION, now).expect("busy").is_empty());
    assert_eq!(bound(&presenter), 0);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, Some(0));
    assert_eq!(presenter.next_wake(now), None);
    assert!(
        presenter
            .advance(GENERATION, now)
            .expect("not polling")
            .is_empty()
    );
    let effects = presenter.completed(admission.run(), now).expect("admitted");
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(
                effect,
                super::Effect::ConsumptionReady {
                    scene: LIVE,
                    index: 0
                }
            ))
            .count(),
        1
    );
    let waiting = jobs(effects);
    assert!(
        presenter
            .consumption_ready(GENERATION, LIVE, 1, now)
            .expect("wrong head")
            .is_empty()
    );
    assert_eq!(bound(&presenter), 0);
    let effects = presenter
        .consumption_ready(GENERATION, LIVE, 0, now)
        .expect("retry");
    assert_eq!(bound(&presenter), 1);
    assert_eq!(presenter.scenes[&LIVE].pending_consumption, None);
    assert_eq!(presenter.returned.load(Ordering::Relaxed), 0);
    assert!(
        presenter
            .consumption_ready(GENERATION, LIVE, 0, now)
            .expect("duplicate")
            .is_empty()
    );
    for job in waiting.into_iter().chain(jobs(effects)) {
        presenter
            .completed(job.run(&mut Shaper), now)
            .expect("next answer");
    }
}

#[test]
fn residency_refusal_is_terminal_and_post_install_pump_failure_is_not_a_retry() {
    let (mut presenter, now) = prepared(&[(RUN, rect("r"))]);
    presenter
        .returned
        .store(MAX_RESIDENT_BYTES, Ordering::Relaxed);
    let effects = presenter
        .advance(GENERATION, now)
        .expect("terminal residency");
    assert_eq!(
        controls(&effects),
        [&ControlToBrain::StepFailed {
            run: RUN,
            scene: LIVE,
            index: 0,
            reason: SceneFailure::Residency,
            cut: kumochi_scene::StopCut {
                activated: 0,
                revealed: kumochi_scene::text::Bounded::default()
            },
        }]
    );
    presenter.returned.store(0, Ordering::Relaxed);
    assert!(
        presenter
            .advance(GENERATION, ms(now, 1000))
            .expect("no retry")
            .is_empty()
    );
    assert_eq!(bound(&presenter), 0);
    assert_eq!(presenter.stoppable_run(), None);

    let (mut presenter, now) = prepared(&[(RUN, rect("r"))]);
    let admission = only_admission(
        arrive(&mut presenter, commit_frame(LIVE, 1, rect("next")), now).expect("lane"),
    );
    presenter.advance(GENERATION, now).expect("busy");
    assert!(
        arrive(
            &mut presenter,
            commit_frame(LIVE, 99, rect("bad-index")),
            now
        )
        .expect("held behind the lane")
        .is_empty()
    );
    let waiting = jobs(
        presenter
            .completed(admission.run(), now)
            .expect("admitted before held input"),
    );
    assert!(matches!(
        presenter.consumption_ready(GENERATION, LIVE, 0, now),
        Err(super::super::Broken::Index)
    ));
    assert_eq!(bound(&presenter), 1, "installation is not rolled back");
    assert_eq!(presenter.scenes[&LIVE].ready.len(), 0);
    assert_eq!(presenter.returned.load(Ordering::Relaxed), 0);
    presenter.disconnected();
    for job in waiting {
        assert!(
            presenter
                .completed(job.run(&mut Shaper), now)
                .expect("late")
                .is_empty()
        );
    }
    assert!(
        presenter
            .advance(GENERATION, now)
            .expect("old generation")
            .is_empty()
    );
    assert_eq!(presenter.take_shown(GENERATION), [None; 4]);
    assert_eq!(presenter.workers(), 0);
}

#[test]
fn sixty_four_lifetime_slots_fit_and_the_sixty_fifth_is_rejected() {
    let (mut presenter, now) = prepared(&[(RUN, rect("s0"))]);
    presenter.advance(GENERATION, now).expect("first");
    for index in 1..64 {
        let mut next = rect(&format!("s{index}"));
        next.start = Start::Now;
        let job =
            only_job(commit_as(&mut presenter, RUN, LIVE, index, next, now).expect("within cap"));
        presenter
            .completed(job.run(&mut Shaper), now)
            .expect("prepared");
        presenter.advance(GENERATION, now).expect("activate");
    }
    assert_eq!(bound(&presenter), 64);
    assert_eq!(presenter.scenes[&LIVE].state.view().revision(), 64);
    assert!(matches!(
        commit_as(&mut presenter, RUN, LIVE, 64, rect("overflow"), now),
        Err(super::super::Broken::Step)
    ));
    assert_eq!(bound(&presenter), 64);
    assert_eq!(presenter.returned.load(Ordering::Relaxed), 0);
}

#[test]
fn independently_begun_scenes_have_distinct_origins_over_the_same_sampler() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let other = SceneId(21);
    begin(&mut presenter, other, ms(now, 100));
    assert_eq!(presenter.scenes[&LIVE].activation.time(ms(now, 300)), 300);
    assert_eq!(presenter.scenes[&other].activation.time(ms(now, 300)), 200);
    presenter.sample_at(ms(now, 300));
    assert_eq!(presenter.scenes[&LIVE].sampled_ms, Some(300));
    assert_eq!(presenter.scenes[&other].sampled_ms, Some(200));
    presenter.sample(400);
    assert_eq!(presenter.scenes[&LIVE].sampled_ms, Some(400));
    assert_eq!(presenter.scenes[&other].sampled_ms, Some(400));
}

#[test]
fn same_bound_motion_and_skipped_delayed_endpoints_invalidate_once_then_rest() {
    let moving = step(
        r#"{"step":{"id":"motion","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10]}},{"anim":{"target":"r","track":"x","ease":"linear","delay":100,"keys":[[0,0],[100,100]]}}]}}"#,
    );
    let (mut presenter, now) = prepared(&[(RUN, moving)]);
    presenter.advance(GENERATION, now).expect("activate");
    for (at, changed, x) in [
        (0, true, 0.0),
        (0, false, 0.0),
        (150, true, 50.0),
        (200, true, 100.0),
        (300, false, 100.0),
    ] {
        assert_eq!(
            presenter.sample_at(ms(now, at)).0.changed,
            changed,
            "at {at}"
        );
        paint_shown(&mut presenter, ms(now, at));
        let mut actual = None;
        assert!(!presenter.frames.borrow().paint(|frame, _| {
            actual = Some(frame.items()[0].transform.0[4]);
            Ok::<_, ()>(())
        }));
        assert_eq!(actual, Some(x));
    }
    assert!(!presenter.animating(ms(now, 300)));
    assert_eq!(presenter.next_wake(ms(now, 300)), None);
    let (mut presenter, now) = prepared(&[(
        RUN,
        step(
            r#"{"step":{"id":"motion","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10]}},{"anim":{"target":"r","track":"x","delay":100,"keys":[[0,0],[100,100]]}}]}}"#,
        ),
    )]);
    presenter.advance(GENERATION, now).expect("activate");
    presenter.sample_at(now);
    paint_shown(&mut presenter, now);
    assert!(
        presenter.sample_at(ms(now, 300)).0.changed,
        "crossed a whole delayed interval"
    );
    assert!(!presenter.sample_at(ms(now, 400)).0.changed);
}

#[test]
fn stop_latches_motion_cancels_a_nonzero_worker_and_ignores_late_results() {
    let moving = step(
        r#"{"step":{"id":"motion","put":[{"shape":{"id":"r","kind":"rect","box":[0,0,10,10]}},{"anim":{"target":"r","track":"x","keys":[[0,0],[1000,100]]}}]}}"#,
    );
    let (mut presenter, now) = prepared(&[(RUN, moving)]);
    presenter.advance(GENERATION, now).expect("activate");
    let worker = only_job(
        commit_as(&mut presenter, RunId(6), LIVE, 1, rect("late"), now).expect("pending worker"),
    );
    assert_eq!(presenter.workers(), 1);
    assert_eq!(presenter.stoppable_run(), Some(RunId(6)));
    assert!(presenter.live_work(now));
    presenter.sample_at(ms(now, 100));
    presenter.stop(RunId(6), ms(now, 100)).expect("first stop");
    assert_eq!(presenter.scenes[&LIVE].cut, Cut::Outstanding);
    let cut = presenter.scenes[&LIVE].cut_now();
    assert_eq!(
        &*cut.revealed,
        &[kumochi_scene::Handle::new(1, 0).expect("the literal retained handle")]
    );
    assert_eq!(
        presenter.workers(),
        1,
        "cancellation is not physical completion"
    );
    assert!(!presenter.sample_at(ms(now, 500)).0.changed);
    assert_eq!(presenter.scenes[&LIVE].sampled_ms, Some(100));
    assert!(!presenter.live_work(ms(now, 500)));
    assert_eq!(presenter.stoppable_run(), None);
    assert_eq!(presenter.next_wake(ms(now, 500)), None);
    presenter
        .clear(ms(now, 500))
        .expect("second clear before acknowledgement");
    assert!(
        presenter
            .completed(worker.run(&mut Shaper), ms(now, 600))
            .expect("late result")
            .is_empty()
    );
    assert_eq!(presenter.workers(), 0);
    assert_eq!(presenter.residency(), 0);
    assert_eq!(presenter.take_shown(GENERATION), [None; 4]);
}

#[test]
fn recovery_origin_cut_gaps_and_saturated_ends_keep_lifetime_identity() {
    let (mut presenter, now, job) = recovering(vec![(LIVE, rect("old"))]);
    presenter
        .completed(job.run(&mut Shaper), ms(now, 100))
        .expect("recovered");
    assert_eq!(presenter.scenes[&LIVE].activation.time(ms(now, 100)), 0);
    assert_eq!(presenter.stoppable_run(), None);
    let after = step(r#"{"step":{"id":"next","start":{"after":"old","ms":200}}}"#);
    let job = only_job(
        commit_as(&mut presenter, RunId(6), LIVE, 1, after, ms(now, 100)).expect("continue"),
    );
    presenter
        .completed(job.run(&mut Shaper), ms(now, 100))
        .expect("ready");
    presenter
        .advance(GENERATION, ms(now, 299))
        .expect("not due");
    assert_eq!(bound(&presenter), 1);
    presenter.advance(GENERATION, ms(now, 300)).expect("due");
    assert_eq!(bound(&presenter), 2);
    let job = only_job(
        commit_as(
            &mut presenter,
            RunId(7),
            LIVE,
            2,
            rect("drop"),
            ms(now, 300),
        )
        .expect("tail"),
    );
    presenter
        .completed(job.run(&mut Shaper), ms(now, 300))
        .expect("tail ready");
    let cut = presenter.scenes[&LIVE].cut_now();
    presenter.stop(RunId(7), ms(now, 300)).expect("stop");
    acknowledge(&mut presenter, LIVE, 3, cut, ms(now, 300)).expect("cut gap");
    let job = only_job(
        commit_as(&mut presenter, RunId(8), LIVE, 3, rect("gap"), ms(now, 400))
            .expect("gap continuation"),
    );
    presenter
        .completed(job.run(&mut Shaper), ms(now, 400))
        .expect("ready");
    presenter
        .advance(GENERATION, ms(now, 1099))
        .expect("prior segment not ended");
    assert_eq!(bound(&presenter), 2);
    presenter
        .advance(GENERATION, ms(now, 1100))
        .expect("kept predecessor ended");
    assert_eq!(bound(&presenter), 4);
    assert_eq!(presenter.scenes[&LIVE].activation.time(ms(now, 1100)), 1000);
    let (mut presenter, now) = prepared(&[
        (RUN, rect("r")),
        (
            RUN,
            step(r#"{"step":{"id":"offset","start":{"after":"r","ms":200}}}"#),
        ),
    ]);
    drop(
        presenter
            .consume_ready(LIVE, 0, now, u64::MAX - 100, 800)
            .expect("large time"),
    );
    let scene = &presenter.scenes[&LIVE];
    assert_eq!(scene.activation.due(scene.state.view(), 1), Ok(u64::MAX));
}
