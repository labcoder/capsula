use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use kumochi_protocol::message::{
    ControlToBrain, DataToShell, RunId, SceneCut, SceneCuts, SceneDescriptorV1, SceneFailure,
    SceneId, SceneSnapshotChunk,
};
use kumochi_scene::{
    Handle, SceneEvent, SceneView, Step, StopCut, deferred::Deferred, layout::SurfaceGeometry,
    text::Bounded, theme::Theme,
};

pub(crate) use super::fixture::*;
use super::{Broken, Completion, Effect, PrepareJob, Presenter, SceneCache, Stage};

/// Stages caches that keep the generation of the view they were staged from.
struct Witness;

struct Witnessed(Arc<()>);

impl Stage for Witness {
    fn fixed_bytes(&self) -> usize {
        0
    }

    fn stage(&self, view: &SceneView) -> Box<dyn SceneCache> {
        Box::new(Witnessed(Arc::clone(view.generation())))
    }
}

impl SceneCache for Witnessed {
    fn bytes(&self) -> usize {
        0
    }
}

/// Stages caches that each hold a clone of one probe, so its count shows where they live.
struct Probed(Arc<()>);

impl Stage for Probed {
    fn fixed_bytes(&self) -> usize {
        0
    }

    fn stage(&self, _: &SceneView) -> Box<dyn SceneCache> {
        Box::new(Probed(Arc::clone(&self.0)))
    }
}

impl SceneCache for Probed {
    fn bytes(&self) -> usize {
        0
    }
}

/// Paints every frame, checking that its slot's cache was staged from the view the frame
/// sampled; returns the item count for every painted frame.
fn bound_item_counts(presenter: &Presenter) -> Option<Vec<usize>> {
    let mut counts = Vec::new();
    let failed = presenter.frames.borrow().paint(|frame, cache| {
        counts.push(frame.items().len());
        let cache: &dyn std::any::Any = cache;
        let staged = cache.downcast_ref::<Witnessed>().map(|cache| &cache.0);
        match (frame.generation(), staged) {
            (Some(sampled), Some(staged)) if Arc::ptr_eq(sampled, staged) => Ok(()),
            _ => Err(()),
        }
    });
    (!failed).then_some(counts)
}

fn bound_everywhere(presenter: &Presenter) -> bool {
    bound_item_counts(presenter).is_some()
}

fn painted_rects(presenter: &Presenter) -> Vec<kumochi_scene::geom::Rect> {
    use kumochi_scene::display::Prim;
    let mut rects = Vec::new();
    assert!(!presenter.frames.borrow().paint(|frame, _| {
        for (item, _, primitives) in frame.paint_items() {
            for primitive in primitives {
                let Prim::Rect { rect, .. } = primitive else {
                    panic!("the fixture paints only rectangles");
                };
                rects.push(item.transform.bounds(*rect));
            }
        }
        Ok::<_, ()>(())
    }));
    rects
}

#[path = "frame_tests.rs"]
mod frames;

#[path = "retirement_tests.rs"]
mod retirement;

#[path = "swap_tests.rs"]
mod swap;

#[path = "screen_tests.rs"]
mod screen;

#[path = "dead_dropper_tests.rs"]
mod dead_dropper;

#[path = "window_tests.rs"]
mod consumption;

#[path = "admit_tests.rs"]
mod admission;

#[path = "paint_tests.rs"]
mod paint_work;

#[path = "stop_tests.rs"]
mod stop;

#[path = "activation_tests.rs"]
mod activation;

#[path = "conduction_tests.rs"]
mod conduction;

#[path = "placement_tests.rs"]
mod placement;

#[test]
#[ignore = "manual quiet-Rez v1 mirror admission timing, not a CI latency assertion"]
#[expect(
    clippy::too_many_lines,
    reason = "construct two legal cap workloads outside one admission timer"
)]
fn measure_v1_mirror_admission_at_caps() {
    use kumochi_protocol::frame::{self, Pipe};

    let shapes =
        (0..300).map(|i| format!(r#"{{"shape":{{"id":"s{i}","kind":"rect","box":[0,0,1,1]}}}}"#));
    let track =
        |i| format!(r#"{{"anim":{{"target":"s{i}","track":"opacity","keys":[[0,0],[60000,1]]}}}}"#);
    let first = step(&format!(
        r#"{{"step":{{"id":"first","put":[{}]}}}}"#,
        shapes
            .chain((0..128).map(track))
            .collect::<Vec<_>>()
            .join(",")
    ));
    let highlights = (0..300)
        .map(|i| format!(r#"{{"highlight":{{"id":"h{i}","target":{{"ref":"s{i}"}}}}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let second = step(&format!(
        r#"{{"step":{{"id":"second","put":[{highlights}]}}}}"#
    ));
    let changes = ["s", "h"]
        .into_iter()
        .flat_map(|prefix| {
            (0..300).map(move |i| format!(r#"{{"target":"{prefix}{i}","color":"red"}}"#))
        })
        .collect::<Vec<_>>()
        .join(",");
    let tracks = (128..256).map(track).collect::<Vec<_>>().join(",");
    let referenced_update = step(&format!(
        r#"{{"step":{{"id":"update","put":[{tracks}],"set":[{changes}]}}}}"#
    ));

    let zeros = std::iter::repeat_n("0", 500).collect::<Vec<_>>().join(",");
    let mut charts = Vec::new();
    let mut patches = Vec::new();
    for chart in 0..4 {
        charts.push(format!(
            r#"{{"chart":{{"id":"c{chart}","kind":"line","box":[0,0,100,100]}}}}"#
        ));
        for series in 0..if chart == 3 { 7 } else { 8 } {
            charts.push(format!(
                r#"{{"series":{{"id":"s{chart}_{series}","in":"c{chart}","data":[{zeros}]}}}}"#
            ));
            patches.push(format!(
                r#"{{"target":"s{chart}_{series}","data":[{zeros}]}}"#
            ));
        }
    }
    let chart_first = step(&format!(
        r#"{{"step":{{"id":"first","put":[{}]}}}}"#,
        charts.join(",")
    ));
    let chart_update = step(&format!(
        r#"{{"step":{{"id":"update","set":[{}]}}}}"#,
        patches.join(",")
    ));

    for (workload, prefix, update, objects) in [
        (
            "objects600_refs300_tracks256",
            vec![first, second],
            referenced_update,
            600,
        ),
        ("chart_points15500", vec![chart_first], chart_update, 35),
    ] {
        let mut samples = Vec::new();
        for iteration in 0..69 {
            let (mut mirror, now) = live();
            begin(&mut mirror, LIVE, now);
            for (index, prior) in prefix.iter().enumerate() {
                commit(
                    &mut mirror,
                    LIVE,
                    u32::try_from(index).expect("prefix index"),
                    prior.clone(),
                    now,
                )
                .expect("legal prefix");
            }
            let index = u32::try_from(prefix.len()).expect("prefix index");
            let message = DataToShell::SceneStepCommit {
                run: RUN,
                scene: LIVE,
                index,
                step: update.clone(),
                speech: None,
            };
            let bytes = u32::try_from(
                frame::encode(&message, Pipe::Data)
                    .expect("bounded typed frame")
                    .len(),
            )
            .expect("frame bytes");
            let start = Instant::now();
            let effects = mirror
                .data(
                    GENERATION,
                    Deferred::new(message, usize::try_from(bytes).expect("frame size")),
                    bytes,
                    now,
                )
                .expect("legal mirrored update");
            let admission = only_admission(effects);
            let receipt = start.elapsed().as_nanos();
            let done = admission.run();
            let start = Instant::now();
            let effects = mirror.completed(done, now).expect("legal mirrored update");
            drop(effects);
            let apply = start.elapsed().as_nanos();
            let elapsed = receipt + apply;
            assert_eq!(mirror.scenes[&LIVE].state.view().object_count(), objects);
            assert_eq!(mirror.scenes[&LIVE].state.view().revision(), index + 1);
            if iteration >= 5 {
                samples.push(elapsed);
                println!(
                    "v1_mirror_admission_parts receipt_ns={receipt} apply_ns={apply} sum_ns={elapsed}; reducer and effect selection, executor dispatch and pipe drain unmeasured"
                );
            }
        }
        samples.sort_unstable();
        println!(
            "v1_mirror_admission workload={workload} samples=64 warmups=5 p50_ns={} p95_ns={} max_ns={}; decoded frame, pending predecessor, no preparation or painting",
            samples[31], samples[60], samples[63]
        );
    }
}

fn label(id: &str, text: &str) -> Step {
    step(&format!(
        r#"{{"step":{{"id":"{id}","put":[{{"text":{{"id":"{id}","at":[10,10],"text":"{text}"}}}}]}}}}"#
    ))
}

fn stop_scenes(cuts: Vec<SceneCut>) -> ControlToBrain {
    ControlToBrain::StopScenes {
        run: RUN,
        cuts: SceneCuts::new(cuts).expect("valid cuts"),
    }
}

fn clear_drawing(scene_ids: Vec<SceneId>) -> ControlToBrain {
    ControlToBrain::ClearDrawing { scene_ids }
}

fn ms(now: Instant, millis: u64) -> Instant {
    now + Duration::from_millis(millis)
}

/// A shown overlay paints every frame, and that paint is observed at `at`.
fn paint_shown(presenter: &mut Presenter, at: Instant) {
    presenter.frames.borrow_mut().shown = true;
    assert!(!presenter.frames.borrow().paint(|_, _| Ok::<_, ()>(())));
    presenter.begin_painted(at);
}

#[test]
fn a_clear_before_membership_survives_a_brain_restart() {
    let mut mirror = presenter();
    let now = Instant::now();
    mirror.clear(now).expect("pending clear");
    mirror.connected(GENERATION + 1);
    let scenes = Bounded::new(vec![
        SceneDescriptorV1::new(LIVE, 1, 1, 1).expect("descriptor"),
    ])
    .expect("one persisted scene");
    let effects = mirror
        .described(GENERATION + 1, &scenes, now)
        .expect("membership");
    assert_eq!(controls(&effects), [&clear_drawing(vec![LIVE])]);
    assert!(!mirror.holds_scenes());
}

#[test]
fn a_clear_without_a_durable_ack_is_resent_when_membership_returns() {
    let (mut mirror, now) = live();
    begin(&mut mirror, LIVE, now);
    commit(&mut mirror, LIVE, 0, rect("a"), now).expect("stored step");
    let effects = mirror.clear(now).expect("local clear");
    assert_eq!(controls(&effects), [&clear_drawing(vec![LIVE])]);
    mirror.connected(GENERATION + 1);
    let scenes = Bounded::new(vec![
        SceneDescriptorV1::new(LIVE, 1, 1, 1).expect("descriptor"),
    ])
    .expect("the brain still holds the scene");
    let effects = mirror
        .described(GENERATION + 1, &scenes, now)
        .expect("membership");
    assert_eq!(controls(&effects), [&clear_drawing(vec![LIVE])]);
    assert!(!mirror.holds_scenes());
}

#[test]
fn a_scene_prepares_its_steps_one_at_a_time_on_the_previous_result() {
    use kumochi_scene::geom::{Rect, Vec2};

    let (mut presenter, now, first) = preparing();
    let replacement = step(
        r#"{"step":{"id":"b","start":"now","remove":["a"],"put":[{"shape":{"id":"b","kind":"rect","box":[20,0,10,10],"enter":"none"}}]}}"#,
    );
    let effects = commit(&mut presenter, LIVE, 1, replacement, now).expect("a matching step");
    assert!(jobs(effects).is_empty());
    assert!(presenter.take_ready().is_empty());
    let ready = |index| ControlToBrain::StepReady {
        run: RUN,
        scene: LIVE,
        index,
    };
    let second = only_job(
        presenter
            .completed(first.run(&mut Shaper), ms(now, 5))
            .expect("first answer"),
    );
    assert_eq!(controls(&presenter.take_ready()), [&ready(0)]);
    presenter
        .completed(second.run(&mut Shaper), ms(now, 10))
        .expect("successor answer");
    assert_eq!(controls(&presenter.take_ready()), [&ready(1)]);
    for (index, time) in [(0, 10), (1, 11)] {
        presenter
            .consume_ready(LIVE, index, ms(now, time), time, 0)
            .expect("consume head");
    }
    presenter
        .sample_at(ms(now, 11))
        .expect("paint the successor");
    assert_eq!(
        painted_rects(&presenter),
        [Rect {
            origin: Vec2::new(20.0, 0.0),
            size: Vec2::new(10.0, 10.0)
        }]
    );
}

#[test]
fn a_commit_at_another_index_ends_the_connection_unprepared() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    assert_eq!(
        commit(&mut presenter, LIVE, 1, rect("a"), now).err(),
        Some(Broken::Index)
    );
}

#[test]
fn a_step_for_an_unknown_scene_ends_the_connection() {
    let (mut presenter, now) = live();
    assert_eq!(
        commit(&mut presenter, LIVE, 0, rect("a"), now).err(),
        Some(Broken::Order)
    );
}

#[test]
fn a_step_the_mirror_rejects_ends_the_connection_unprepared() {
    let (mut presenter, now, _job) = preparing();
    assert_eq!(
        commit(&mut presenter, LIVE, 1, rect("a"), now).err(),
        Some(Broken::Step)
    );
}

#[test]
fn each_prepared_step_is_announced_once_and_a_refused_or_late_one_never() {
    let ready = |index| ControlToBrain::StepReady {
        run: RUN,
        scene: LIVE,
        index,
    };
    let (mut presenter, now, first) = preparing();
    let effects =
        commit(&mut presenter, LIVE, 1, label("b", "unshapeable"), now).expect("a matching step");
    assert!(jobs(effects).is_empty());
    let second = only_job(
        presenter
            .completed(first.run(&mut Shaper), ms(now, 5))
            .expect("a current answer"),
    );
    assert_eq!(controls(&presenter.take_ready()), [&ready(0)]);
    assert!(presenter.take_ready().is_empty());
    let effects = presenter
        .completed(second.run(&mut Shaper), ms(now, 10))
        .expect("an answer");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 1, SceneFailure::TextMeasure)]
    );
    assert!(presenter.take_ready().is_empty());

    let (mut presenter, now, job) = preparing();
    presenter.tick(ms(now, 250)).expect("a tick");
    presenter
        .completed(job.run(&mut Shaper), ms(now, 400))
        .expect("a late answer");
    assert!(presenter.take_ready().is_empty());

    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let bad = only_job(
        commit(&mut presenter, LIVE, 0, label("a", "unshapeable"), now).expect("first step"),
    );
    let effects = presenter
        .completed(bad.run(&mut Shaper), now)
        .expect("refused");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 0, SceneFailure::TextMeasure)]
    );
    assert!(presenter.take_ready().is_empty());
    let effects = presenter
        .stop(RUN, now)
        .expect("the failed first step was already cut");
    assert_eq!(controls(&effects), [&stop_scenes(Vec::new())]);
}

#[test]
fn a_scene_is_cut_at_most_once_between_commits() {
    let (mut presenter, now, _job) = preparing();
    presenter.tick(ms(now, 250)).expect("a tick");
    let effects = presenter.stop(RUN, ms(now, 300)).expect("a stop");
    assert_eq!(controls(&effects), [&stop_scenes(Vec::new())]);
}

#[test]
fn a_stopped_run_s_steps_reach_the_mirror_unprepared_until_the_cut_is_acknowledged() {
    let (mut presenter, now, job) = preparing();
    let effects = presenter.stop(RUN, now).expect("a stop");
    let cut = SceneCut {
        scene: LIVE,
        cut: zero_cut(),
    };
    assert_eq!(controls(&effects), [&stop_scenes(vec![cut])]);
    assert!(job.cancelled());
    let effects = commit(&mut presenter, LIVE, 1, rect("b"), now).expect("a fenced step");
    assert!(jobs(effects).is_empty());
    let late = presenter
        .completed(job.run(&mut Shaper), ms(now, 5))
        .expect("a stale answer");
    assert!(late.is_empty());
    let effects = acknowledge(&mut presenter, LIVE, 2, zero_cut(), now).expect("the cut");
    assert_eq!(credit(&effects), FRAME);
    assert!(controls(&effects).is_empty());
    assert!(!presenter.holds_scenes());
    let effects = commit(&mut presenter, LIVE, 2, rect("c"), now).expect("a cleared scene");
    assert_eq!(credit(&effects), FRAME);
    assert!(jobs(effects).is_empty());
}

#[test]
fn an_acknowledgement_before_the_fenced_steps_arrive_ends_the_connection() {
    let (mut presenter, now, _job) = preparing();
    presenter.stop(RUN, now).expect("a stop");
    commit(&mut presenter, LIVE, 1, rect("b"), now).expect("a fenced step");
    assert_eq!(
        acknowledge(&mut presenter, LIVE, 1, zero_cut(), now).err(),
        Some(Broken::Cut)
    );
}

#[test]
fn a_cut_for_a_scene_the_shell_no_longer_holds_is_dropped() {
    let (mut presenter, now) = live();
    let effects = acknowledge(&mut presenter, LIVE, 3, zero_cut(), now).expect("a stale cut");
    assert_eq!(credit(&effects), FRAME);
    assert!(controls(&effects).is_empty());
}

#[test]
fn clear_releases_every_scene_its_charge_and_its_running_work() {
    let (mut presenter, now, first) = preparing();
    let before = presenter.residency();
    commit(&mut presenter, LIVE, 1, rect("b"), now).expect("a matching step");
    let second = only_job(
        presenter
            .completed(first.run(&mut Shaper), ms(now, 5))
            .expect("a current answer"),
    );
    assert!(presenter.residency() > before);
    let effects = presenter.clear(ms(now, 6)).expect("a clear");
    assert_eq!(controls(&effects), [&clear_drawing(vec![LIVE])]);
    assert!(second.cancelled());
    assert_eq!(presenter.residency(), 0);
    assert!(!presenter.holds_scenes());
    let late = presenter
        .completed(second.run(&mut Shaper), ms(now, 7))
        .expect("a stale answer");
    assert!(late.is_empty());
    let effects = commit(&mut presenter, LIVE, 2, rect("c"), now).expect("a cleared scene");
    assert!(jobs(effects).is_empty());
}

#[test]
fn a_reconnect_cancels_every_job_and_drops_the_old_connection_s_frames() {
    let (mut presenter, now, job) = preparing();
    presenter.connected(GENERATION + 1);
    assert!(job.cancelled());
    assert_eq!(presenter.residency(), 0);
    let late = presenter
        .completed(job.run(&mut Shaper), ms(now, 5))
        .expect("a stale answer");
    assert!(late.is_empty());
    let old = commit(&mut presenter, LIVE, 1, rect("b"), now).expect("an old frame");
    assert!(old.is_empty());
}

fn kept(scene: SceneId) -> SceneCut {
    let revealed = vec![Handle::new(1, 0).expect("the first step's first object")];
    SceneCut {
        scene,
        cut: StopCut {
            activated: 1,
            revealed: Bounded::new(revealed).expect("one handle"),
        },
    }
}

#[test]
fn a_cut_past_the_rest_s_bound_still_clears_the_scene() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    drop(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("the first step"));
    drop(commit(&mut presenter, LIVE, 1, rect("b"), now).expect("the second step"));
    let revealed = [(1, 0), (2, 0)]
        .map(|(revision, slot)| Handle::new(revision, slot).expect("a committed object"))
        .to_vec();
    let cut = StopCut {
        activated: 2,
        revealed: Bounded::new(revealed).expect("two handles"),
    };
    let effects = acknowledge(&mut presenter, LIVE, 2, cut, now).expect("a cut the brain applied");
    assert_eq!(controls(&effects), [&clear_drawing(vec![LIVE])]);
    assert!(presenter.scenes.is_empty());
}

#[test]
fn live_frames_wait_for_recovery_and_then_apply_in_arrival_order() {
    let recovered = SceneId(10);
    let (mut presenter, now, job) = recovering(vec![(recovered, rect("r"))]);
    let held = begin(&mut presenter, LIVE, now);
    assert!(held.is_empty());
    let held = commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a held step");
    assert!(held.is_empty());
    let effects = presenter
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("the batch");
    let effects = settle(&mut presenter, effects, ms(now, 20)).expect("the held step");
    assert_eq!(credit(&effects), 2 * FRAME);
    assert_eq!(only_job(effects).scene(), LIVE);
    let effects = presenter.stop(RUN, ms(now, 30)).expect("a stop");
    let fresh = SceneCut {
        scene: LIVE,
        cut: zero_cut(),
    };
    assert_eq!(
        controls(&effects),
        [&stop_scenes(vec![kept(recovered), fresh])]
    );
}

#[test]
fn a_recovered_scene_that_fails_is_cleared_while_the_others_recover() {
    let (good, bad) = (SceneId(10), SceneId(11));
    let (mut presenter, now, job) =
        recovering(vec![(good, rect("r")), (bad, label("t", "unshapeable"))]);
    let effects = presenter
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("the batch");
    assert_eq!(controls(&effects), [&clear_drawing(vec![bad])]);
    let effects = presenter.stop(RUN, ms(now, 30)).expect("a stop");
    assert_eq!(controls(&effects), [&stop_scenes(vec![kept(good)])]);
}

#[test]
fn the_recovery_deadline_clears_the_batch_and_its_late_answer_installs_nothing() {
    let recovered = SceneId(10);
    let (mut presenter, now, job) = recovering(vec![(recovered, rect("r"))]);
    assert_eq!(presenter.deadline(), Some(ms(now, 2_000)));
    let effects = presenter.tick(ms(now, 2_000)).expect("a tick");
    assert_eq!(controls(&effects), [&clear_drawing(vec![recovered])]);
    assert!(job.cancelled());
    let late = presenter
        .completed(job.run(&mut Shaper), ms(now, 2_500))
        .expect("a late batch");
    assert!(late.is_empty());
    assert!(!presenter.holds_scenes());
}

#[test]
fn stop_clears_an_incomplete_recovery_before_it_cuts() {
    let mut presenter = presenter();
    let now = Instant::now();
    let recovering = SceneId(10);
    described(&mut presenter, &[recovering], now);
    let effects = presenter.stop(RUN, now).expect("a stop");
    assert_eq!(
        controls(&effects),
        [&clear_drawing(vec![recovering]), &stop_scenes(Vec::new())]
    );
}

/// Eight cleared scenes whose cancelled workers never answer, holding every worker slot.
fn exhausted() -> (Presenter, Instant, Vec<PrepareJob>) {
    let (mut presenter, now) = live();
    let mut zombies = Vec::new();
    for scene in 30..38 {
        let scene = SceneId(scene);
        begin(&mut presenter, scene, now);
        let job = only_job(commit(&mut presenter, scene, 0, rect("a"), now).expect("a step"));
        presenter.clear(now).expect("a clear");
        assert!(job.cancelled());
        zombies.push(job);
    }
    (presenter, now, zombies)
}

#[test]
fn an_admission_that_finds_every_worker_slot_held_clears_its_scene_at_its_deadline() {
    let (mut presenter, now, _zombies) = exhausted();
    begin(&mut presenter, LIVE, now);
    let effects = commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step");
    assert!(effects.is_empty(), "no slot and no credit yet");
    assert_eq!(presenter.deadline(), Some(ms(now, 250)));
    assert!(presenter.tick(ms(now, 249)).expect("a tick").is_empty());
    let effects = presenter.tick(ms(now, 250)).expect("a tick");
    assert_eq!(credit(&effects), FRAME);
    assert_eq!(
        controls(&effects),
        [&ControlToBrain::SceneFailed { scene: LIVE }]
    );
    assert!(!presenter.holds_scenes());
    assert_eq!(presenter.deadline(), None);
    let late = commit(&mut presenter, LIVE, 0, rect("b"), now).expect("a cleared scene");
    assert_eq!(credit(&late), FRAME);
    assert!(controls(&late).is_empty());
}

#[test]
fn a_freed_worker_slot_starts_the_waiting_admission_on_the_same_clock() {
    let (mut presenter, now, mut zombies) = exhausted();
    begin(&mut presenter, LIVE, now);
    commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step");
    let zombie = zombies.pop().expect("a held slot");
    let effects = presenter
        .completed(zombie.run(&mut Shaper), ms(now, 249))
        .expect("a stale answer");
    let admission = only_admission(effects);
    assert_eq!(presenter.deadline(), Some(ms(now, 250)));
    let effects = presenter
        .completed(admission.run(), ms(now, 249))
        .expect("an admission");
    assert_eq!(credit(&effects), FRAME);
    assert_eq!(only_job(effects).scene(), LIVE);
    assert_eq!(
        presenter.deadline(),
        Some(ms(now, 250)),
        "the step was eligible from its commit"
    );
}

#[test]
fn a_slot_freed_at_or_after_the_waiting_admission_s_deadline_clears_its_scene() {
    for at in [250, 251] {
        let (mut presenter, now, mut zombies) = exhausted();
        begin(&mut presenter, LIVE, now);
        commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step");
        let zombie = zombies.pop().expect("a held slot");
        let effects = presenter
            .completed(zombie.run(&mut Shaper), ms(now, at))
            .expect("a stale answer");
        assert_eq!(credit(&effects), FRAME, "a slot freed at {at} ms");
        assert_eq!(
            controls(&effects),
            [&ControlToBrain::SceneFailed { scene: LIVE }],
            "a slot freed at {at} ms"
        );
        assert!(admissions(effects).is_empty(), "a slot freed at {at} ms");
        assert_eq!(presenter.deadline(), None, "a slot freed at {at} ms");
    }
}

/// Seven zombie slots, and LIVE's admitted step queued for the eighth: an earlier scene's
/// admission took it first, and that scene's preparation, returned here, holds it now.
fn queued() -> (Presenter, Instant, Vec<PrepareJob>, PrepareJob) {
    let (mut presenter, now, mut zombies) = exhausted();
    let earlier = SceneId(19);
    begin(&mut presenter, earlier, now);
    begin(&mut presenter, LIVE, now);
    let zombie = zombies.pop().expect("a held slot");
    let freed = presenter
        .completed(zombie.run(&mut Shaper), now)
        .expect("a stale answer");
    assert!(freed.is_empty());
    let frame = commit_frame(LIVE, 0, rect("queued"));
    let admission = only_admission(arrive(&mut presenter, frame, now).expect("a step"));
    let frame = commit_frame(earlier, 0, rect("e"));
    assert!(
        arrive(&mut presenter, frame, now)
            .expect("a held step")
            .is_empty()
    );
    let effects = presenter
        .completed(admission.run(), now)
        .expect("LIVE's admission");
    let admission = only_admission(effects);
    let effects = presenter
        .completed(admission.run(), now)
        .expect("the earlier admission");
    let holder = only_job(effects);
    assert_eq!(holder.scene(), earlier);
    assert_eq!(presenter.scenes[&LIVE].queue.len(), 1);
    assert_eq!(presenter.workers(), 8);
    (presenter, now, zombies, holder)
}

#[test]
fn a_step_that_finds_every_worker_slot_held_fails_at_its_deadline() {
    let (mut presenter, now, _zombies, _holder) = queued();
    assert_eq!(presenter.deadline(), Some(ms(now, 250)));
    let effects = presenter.tick(ms(now, 250)).expect("a tick");
    assert_eq!(
        controls(&effects),
        [
            &failed(SceneId(19), 0, SceneFailure::Timeout),
            &failed(LIVE, 0, SceneFailure::Timeout)
        ]
    );
    assert_eq!(presenter.deadline(), None);
}

#[test]
fn a_step_s_deadline_excludes_the_time_it_waited_for_its_predecessors() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    for (index, id) in [(1, "b"), (2, "c")] {
        let effects = commit(&mut presenter, LIVE, index, rect(id), now).expect("a step");
        assert!(jobs(effects).is_empty());
    }
    let second = only_job(
        presenter
            .completed(first.run(&mut Shaper), ms(now, 200))
            .expect("an answer"),
    );
    let third = only_job(
        presenter
            .completed(second.run(&mut Shaper), ms(now, 400))
            .expect("an answer"),
    );
    assert_eq!(third.scene(), LIVE);
    assert_eq!(presenter.deadline(), Some(ms(now, 650)));
}

/// A reconnect whose one-scene batch is assembled while eight zombie jobs hold every slot.
fn recovering_exhausted() -> (Presenter, Instant, Vec<PrepareJob>, SceneId) {
    let (mut presenter, now, zombies) = exhausted();
    let recovered = SceneId(10);
    presenter.connected(GENERATION + 1);
    let descriptor = SceneDescriptorV1::new(recovered, 1, 1, 1).expect("counts");
    let descriptors = Bounded::new(vec![descriptor]).expect("one scene");
    presenter
        .described(GENERATION + 1, &descriptors, now)
        .expect("membership");
    let effects = presenter
        .data(
            GENERATION + 1,
            Deferred::new(snapshot(recovered, rect("r")), 0),
            FRAME,
            now,
        )
        .expect("a chunk");
    assert!(controls(&effects).is_empty());
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Recover(_)))
    );
    assert!(presenter.holds_scenes());
    assert_eq!(presenter.deadline(), Some(ms(now, 2_000)));
    (presenter, now, zombies, recovered)
}

#[test]
fn a_recovery_waiting_for_a_worker_slot_starts_when_one_frees_before_its_deadline() {
    let (mut presenter, now, mut zombies, recovered) = recovering_exhausted();
    let zombie = zombies.pop().expect("a held slot");
    let effects = presenter
        .completed(zombie.run(&mut Shaper), ms(now, 1_000))
        .expect("a stale answer");
    assert!(controls(&effects).is_empty());
    let job = recovery(effects);
    assert_eq!(presenter.deadline(), Some(ms(now, 2_000)));
    let effects = presenter
        .completed(job.run(&mut Shaper), ms(now, 1_100))
        .expect("the batch");
    assert!(controls(&effects).is_empty());
    let effects = presenter.stop(RUN, ms(now, 1_200)).expect("a stop");
    assert_eq!(controls(&effects), [&stop_scenes(vec![kept(recovered)])]);
}

#[test]
fn a_slot_freed_at_or_after_a_waiting_recovery_s_deadline_clears_its_batch() {
    for at in [2_000, 2_001] {
        let (mut presenter, now, mut zombies, recovered) = recovering_exhausted();
        let zombie = zombies.pop().expect("a held slot");
        let effects = presenter
            .completed(zombie.run(&mut Shaper), ms(now, at))
            .expect("a stale answer");
        assert_eq!(
            controls(&effects),
            [&clear_drawing(vec![recovered])],
            "a slot freed at {at} ms"
        );
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::Recover(_))),
            "a slot freed at {at} ms"
        );
        assert!(!presenter.holds_scenes(), "a slot freed at {at} ms");
        assert_eq!(presenter.deadline(), None, "a slot freed at {at} ms");
    }
}

#[test]
fn a_recovery_answer_at_or_after_its_deadline_installs_nothing_even_before_the_watchdog() {
    for at in [2_000, 2_001] {
        let recovered = SceneId(10);
        let (mut presenter, now, job) = recovering(vec![(recovered, rect("r"))]);
        let effects = presenter
            .completed(job.run(&mut Shaper), ms(now, at))
            .expect("a late batch");
        assert_eq!(
            controls(&effects),
            [&clear_drawing(vec![recovered])],
            "an answer at {at} ms"
        );
        assert!(!presenter.holds_scenes(), "an answer at {at} ms");
    }
}

#[test]
fn a_recovery_still_waiting_for_a_worker_slot_at_its_deadline_clears_its_batch() {
    let (mut presenter, now, _zombies, recovered) = recovering_exhausted();
    let effects = presenter.tick(ms(now, 2_000)).expect("a tick");
    assert_eq!(controls(&effects), [&clear_drawing(vec![recovered])]);
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Recover(_)))
    );
    assert!(!presenter.holds_scenes());
    assert_eq!(presenter.deadline(), None);
}

#[test]
fn a_job_dropped_unspawned_frees_its_worker_slot() {
    let (mut presenter, now) = live();
    for scene in 30..38 {
        let scene = SceneId(scene);
        begin(&mut presenter, scene, now);
        drop(commit(&mut presenter, scene, 0, rect("a"), now).expect("a step"));
        presenter.clear(now).expect("a clear");
    }
    assert_eq!(presenter.workers(), 0);
    begin(&mut presenter, LIVE, now);
    let effects = commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step");
    assert_eq!(only_job(effects).scene(), LIVE);
}

#[test]
fn a_job_discarded_with_a_broken_batch_frees_its_worker_slot() {
    let (mut presenter, now, job) = recovering(vec![(SceneId(10), rect("r"))]);
    begin(&mut presenter, LIVE, now);
    commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a held step");
    commit(&mut presenter, LIVE, 5, rect("b"), now).expect("a held step");
    let effects = presenter
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("the batch and the first held step's admission");
    let broken = settle(&mut presenter, effects, ms(now, 20)).err();
    assert_eq!(broken, Some(Broken::Index));
    assert_eq!(presenter.workers(), 0);
}

#[test]
fn a_stopped_run_s_late_step_on_an_already_cut_scene_gets_one_new_cut() {
    let recovered = SceneId(10);
    let (mut presenter, now, job) = recovering(vec![(recovered, rect("r"))]);
    presenter
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("the batch");
    let failing = only_job(
        commit(&mut presenter, recovered, 1, label("t", "unshapeable"), now).expect("a step"),
    );
    let effects = presenter
        .completed(failing.run(&mut Shaper), ms(now, 30))
        .expect("an answer");
    let at_rest = kept(recovered).cut;
    let step_failed = |index, reason| ControlToBrain::StepFailed {
        run: RUN,
        scene: recovered,
        index,
        reason,
        cut: at_rest.clone(),
    };
    assert_eq!(
        controls(&effects),
        [&step_failed(1, SceneFailure::TextMeasure)]
    );
    acknowledge(&mut presenter, recovered, 2, at_rest.clone(), now).expect("the cut");
    let effects = presenter.stop(RUN, ms(now, 40)).expect("a stop");
    assert_eq!(controls(&effects), [&stop_scenes(Vec::new())]);
    let effects = commit(&mut presenter, recovered, 2, rect("b"), now).expect("a late step");
    assert_eq!(
        controls(&effects),
        [&step_failed(2, SceneFailure::Cancelled)]
    );
    assert!(jobs(effects).is_empty());
    acknowledge(&mut presenter, recovered, 3, at_rest.clone(), now).expect("the new cut");
    let later = RunId(RUN.0 + 1);
    let effects =
        commit_as(&mut presenter, later, recovered, 3, rect("c"), now).expect("a continuation");
    assert_eq!(only_job(effects).scene(), recovered);
}

#[test]
fn a_recovered_scene_whose_history_ends_in_a_cut_gets_no_cut_until_a_new_commit() {
    let recovered = SceneId(10);
    let at_rest = kept(recovered).cut;
    let mut presenter = presenter();
    let now = Instant::now();
    let descriptor = SceneDescriptorV1::new(recovered, 2, 1, 3).expect("counts");
    let descriptors = Bounded::new(vec![descriptor]).expect("one scene");
    let effects = presenter
        .described(GENERATION, &descriptors, now)
        .expect("membership");
    assert!(effects.is_empty());
    let events = vec![
        SceneEvent::Step {
            revision: 1,
            step: Arc::new(rect("a")),
        },
        SceneEvent::Step {
            revision: 2,
            step: Arc::new(rect("b")),
        },
        SceneEvent::Stop(at_rest.clone()),
    ];
    let chunk = DataToShell::SceneSnapshotV1 {
        chunk: SceneSnapshotChunk {
            scene: recovered,
            revision: 2,
            ordinal: 0,
            header: Some(header()),
            events: Bounded::new(events).expect("three events"),
            last: true,
        },
    };
    let job = recovery(data(&mut presenter, chunk, now).expect("a chunk"));
    presenter
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("the batch");
    let effects = presenter.stop(RUN, ms(now, 30)).expect("a stop");
    assert_eq!(controls(&effects), [&stop_scenes(Vec::new())]);
    let effects = presenter.stop(RUN, ms(now, 31)).expect("a second stop");
    assert_eq!(controls(&effects), [&stop_scenes(Vec::new())]);
    let effects = commit(&mut presenter, recovered, 2, rect("c"), now).expect("a late step");
    assert_eq!(
        controls(&effects),
        [&ControlToBrain::StepFailed {
            run: RUN,
            scene: recovered,
            index: 2,
            reason: SceneFailure::Cancelled,
            cut: at_rest.clone(),
        }]
    );
    assert!(jobs(effects).is_empty());
    acknowledge(&mut presenter, recovered, 3, at_rest, now).expect("the new cut");
    let later = RunId(RUN.0 + 1);
    let effects =
        commit_as(&mut presenter, later, recovered, 3, rect("d"), now).expect("a continuation");
    assert_eq!(only_job(effects).scene(), recovered);
}

#[test]
fn a_finished_answer_holds_its_worker_slot_until_it_is_applied_or_dropped() {
    let (mut presenter, now, job) = preparing();
    let done = job.run(&mut Shaper);
    assert_eq!(presenter.workers(), 1);
    presenter.completed(done, ms(now, 5)).expect("an answer");
    assert_eq!(presenter.workers(), 0);
    let job = only_job(commit(&mut presenter, LIVE, 1, rect("b"), ms(now, 6)).expect("a step"));
    let undelivered = job.run(&mut Shaper);
    assert_eq!(presenter.workers(), 1);
    drop(undelivered);
    assert_eq!(presenter.workers(), 0);
}

#[test]
fn jobs_answers_and_the_presenter_print_no_drawing_content() {
    let private = step(
        r#"{"step":{"id":"zqxprivateid","put":[{"text":{"id":"zqxprivateid","at":[13337.25,10],"text":"zqxprivatelabel"}}]}}"#,
    );
    let leaks = |printed: &str| {
        ["13337", "zqxprivate"]
            .iter()
            .any(|sentinel| printed.contains(sentinel))
    };
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let frame = commit_frame(LIVE, 0, private.clone());
    let effects = arrive(&mut presenter, frame, now).expect("a step");
    let printed = format!("{effects:?}");
    assert!(printed.contains("Admit"), "{printed}");
    assert!(!leaks(&printed), "{printed}");
    let done = only_admission(effects).run();
    let printed = format!("{done:?}");
    assert!(!leaks(&printed), "{printed}");
    let effects = presenter.completed(done, now).expect("an admission");
    let printed = format!("{effects:?}");
    assert!(printed.contains("Prepare"), "{printed}");
    assert!(!leaks(&printed), "{printed}");
    let done = only_job(effects).run(&mut Shaper);
    let printed = format!("{done:?}");
    assert!(!leaks(&printed), "{printed}");
    presenter.completed(done, ms(now, 5)).expect("an answer");
    let printed = format!("{presenter:?}");
    assert!(!leaks(&printed), "{printed}");

    let mut presenter = self::presenter();
    let recovered = SceneId(10);
    described(&mut presenter, &[recovered], now);
    let effects = data(&mut presenter, snapshot(recovered, private), now).expect("a chunk");
    let printed = format!("{effects:?}");
    assert!(printed.contains("Recover"), "{printed}");
    assert!(!leaks(&printed), "{printed}");
    let done = recovery(effects).run(&mut Shaper);
    let printed = format!("{done:?}");
    assert!(!leaks(&printed), "{printed}");
    presenter.completed(done, ms(now, 20)).expect("the batch");
    let printed = format!("{presenter:?}");
    assert!(!leaks(&printed), "{printed}");
}

fn first_step(frame: &DataToShell) -> Arc<Step> {
    let DataToShell::SceneSnapshotV1 { chunk } = frame else {
        panic!("a snapshot frame");
    };
    let SceneEvent::Step { step, .. } = &chunk.events[0] else {
        panic!("a step event");
    };
    Arc::clone(step)
}

#[test]
fn stale_held_and_discarded_snapshot_data_drops_on_the_dropper() {
    let scene = SceneId(10);
    let mut presenter = presenter();
    let now = Instant::now();
    let release = hold_dropper();
    let stale = snapshot(scene, rect("r"));
    let stale_step = first_step(&stale);
    let effects = presenter
        .data(GENERATION + 1, Deferred::new(stale, 0), FRAME, now)
        .expect("a stale frame");
    assert!(effects.is_empty(), "a stale frame earns nothing");
    let held = snapshot(scene, rect("r"));
    let held_step = first_step(&held);
    let effects = data(&mut presenter, held, now).expect("a chunk before membership");
    assert!(effects.is_empty(), "a chunk before membership is held");
    presenter.disconnected();
    presenter.connected(GENERATION);
    described(&mut presenter, &[scene], now);
    let batch = snapshot(scene, rect("r"));
    let batch_step = first_step(&batch);
    let effects = data(&mut presenter, batch, now).expect("a chunk");
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Recover(_))),
        "the batch is sent for recovery"
    );
    drop(effects);
    for (step, owners) in [(&stale_step, 2), (&held_step, 2), (&batch_step, 3)] {
        assert_eq!(
            Arc::strong_count(step),
            owners,
            "a discarded frame, log or batch waits for the dropper"
        );
    }
    drop(release);
    for step in [&stale_step, &held_step, &batch_step] {
        assert!(drained(step, 1), "the dropper destroys it");
    }
}

#[test]
fn rejected_undelivered_and_discarded_preparations_drop_on_the_dropper() {
    let theme = Arc::new(Theme::default());
    let surface = SurfaceGeometry::Overlay {
        default_canvas: [1920.0, 1080.0],
    };
    let mut presenter = Presenter::new(surface, Arc::clone(&theme), Box::new(Charged(0)));
    let release = hold_dropper();
    presenter.connected(GENERATION);
    let now = Instant::now();
    let effects = presenter
        .described(GENERATION, &Bounded::default(), now)
        .expect("empty membership");
    assert!(effects.is_empty(), "no scene to recover");
    let owners = Arc::strong_count(&theme);
    let (discarded, undelivered) = (SceneId(21), SceneId(22));
    for scene in [LIVE, discarded, undelivered] {
        begin(&mut presenter, scene, now);
    }
    let first = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step"));
    presenter
        .completed(first.run(&mut Shaper), ms(now, 5))
        .expect("a current answer");
    let second = commit(&mut presenter, LIVE, 1, rect("b"), ms(now, 6)).expect("a step");
    let second = only_job(second);
    let answer = second.run(&mut Shaper);
    let before = Arc::strong_count(&theme);
    let effects = presenter
        .completed(answer, ms(now, 256))
        .expect("a late answer");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 1, SceneFailure::Timeout)],
        "the late step fails and its prepared predecessor is dropped"
    );
    assert_eq!(
        Arc::strong_count(&theme),
        before,
        "the late answer and the halted prepared step wait for the dropper"
    );
    let effects = commit(&mut presenter, discarded, 0, rect("c"), now).expect("a step");
    let before = Arc::strong_count(&theme);
    drop(effects);
    assert_eq!(
        Arc::strong_count(&theme),
        before,
        "a discarded job's input waits for the dropper"
    );
    let job = only_job(commit(&mut presenter, undelivered, 0, rect("d"), now).expect("a step"));
    let answer = job.run(&mut Shaper);
    let before = Arc::strong_count(&theme);
    let (sender, receiver) = mpsc::channel();
    drop(receiver);
    drop(
        sender
            .send(answer)
            .expect_err("the completion recipient is gone"),
    );
    assert_eq!(
        Arc::strong_count(&theme),
        before,
        "an undelivered answer waits for the dropper"
    );
    drop(release);
    assert!(drained(&theme, owners), "the dropper destroys them all");
}

#[test]
fn a_kept_cut_charges_its_rest_once_and_releases_its_prepared_steps() {
    let recovered = SceneId(10);
    let (mut presenter, now, job) = recovering(vec![(recovered, rect("r"))]);
    presenter
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("the batch");
    let rest = presenter.residency();
    assert!(rest > 0, "the recovered rest is charged");
    let job = only_job(commit(&mut presenter, recovered, 1, rect("b"), now).expect("a step"));
    let done = job.run(&mut Shaper);
    let Completion::Prepared { result, .. } = &done else {
        panic!("a prepared step");
    };
    let Ok(prepared) = &**result else {
        panic!("a prepared step");
    };
    let charged = prepared.resident_bytes;
    presenter.completed(done, ms(now, 30)).expect("an answer");
    assert_eq!(
        presenter.residency(),
        rest + charged,
        "an adopted step adds its own charge"
    );
    let effects = presenter.stop(RUN, ms(now, 40)).expect("a stop");
    assert_eq!(controls(&effects), [&stop_scenes(vec![kept(recovered)])]);
    assert_eq!(
        presenter.residency(),
        rest,
        "a stop releases the prepared step"
    );
    acknowledge(&mut presenter, recovered, 2, kept(recovered).cut, now).expect("the cut");
    assert_eq!(
        presenter.residency(),
        rest,
        "the kept rest replaces the old one and is charged once"
    );
}

#[test]
fn recovery_transfers_a_full_rest_charge_not_just_the_last_ready_delta() {
    let (mut mirror, now) = live();
    begin(&mut mirror, LIVE, now);
    let first = only_job(commit(&mut mirror, LIVE, 0, rect("r"), now).expect("first step"));
    mirror
        .completed(first.run(&mut Shaper), ms(now, 1))
        .expect("first answer");
    let empty = step(r#"{"step":{"id":"empty"}}"#);
    let second =
        only_job(commit(&mut mirror, LIVE, 1, empty.clone(), ms(now, 2)).expect("second step"));
    let done = second.run(&mut Shaper);
    let Completion::Prepared { result, .. } = &done else {
        panic!("a prepared suffix");
    };
    let delta = result.as_ref().expect("prepared suffix").resident_bytes;
    let mut recovered = presenter();
    let named = Bounded::new(vec![SceneDescriptorV1::new(LIVE, 2, 2, 2).expect("counts")])
        .expect("one scene");
    recovered
        .described(GENERATION, &named, now)
        .expect("membership");
    let frame = DataToShell::SceneSnapshotV1 {
        chunk: SceneSnapshotChunk {
            scene: LIVE,
            revision: 2,
            ordinal: 0,
            header: Some(header()),
            events: Bounded::new(vec![
                SceneEvent::Step {
                    revision: 1,
                    step: Arc::new(rect("r")),
                },
                SceneEvent::Step {
                    revision: 2,
                    step: Arc::new(empty),
                },
            ])
            .expect("two events"),
            last: true,
        },
    };
    let job = recovery(data(&mut recovered, frame, now).expect("snapshot"));
    recovered
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("recovery answer");
    assert_eq!(recovered.scenes[&LIVE].state.view().revision(), 2);
    assert_eq!(recovered.scenes[&LIVE].state.view().object_count(), 1);
    assert!(recovered.scenes[&LIVE].ready.is_empty());
    assert!(
        recovered.residency() > delta,
        "the rest also owns the earlier rectangle and its source"
    );
    let retained = recovered.residency();
    recovered
        .stop(RUN, ms(now, 30))
        .expect("stop at the recovered rest");
    assert_eq!(
        recovered.residency(),
        retained,
        "dropping unactivated work cannot release the rest's charge"
    );
}

#[test]
fn queued_tickets_survive_halt_expiry_and_input_errors_until_retirement() {
    for case in 0..3 {
        let (mut presenter, now, mut zombies, _holder) = queued();
        let SceneEvent::Step { step, .. } = &presenter.scenes[&LIVE].state.view().recovery_log()[0]
        else {
            panic!("the queued step is recorded");
        };
        let probe = Arc::clone(step);
        assert!(drained(&probe, 4));
        assert_eq!(
            Arc::strong_count(&probe),
            4,
            "log, aliased ticket inputs and probe"
        );
        let release = hold_dropper();
        let effects = if case == 0 {
            presenter.stop(RUN, now).expect("halt")
        } else {
            if case == 2 {
                presenter.surface = SurfaceGeometry::Inline { width: -1.0 };
            }
            let zombie = zombies.pop().expect("a slot");
            presenter
                .completed(
                    zombie.run(&mut Shaper),
                    ms(now, if case == 1 { 250 } else { 100 }),
                )
                .expect("the old completion frees a slot")
        };
        let expected = match case {
            0 => stop_scenes(vec![
                SceneCut {
                    scene: SceneId(19),
                    cut: zero_cut(),
                },
                SceneCut {
                    scene: LIVE,
                    cut: zero_cut(),
                },
            ]),
            1 => failed(LIVE, 0, SceneFailure::Timeout),
            _ => failed(LIVE, 0, SceneFailure::Layout),
        };
        assert_eq!(controls(&effects), [&expected]);
        assert_eq!(
            Arc::strong_count(&probe),
            4,
            "the removed ticket stays protected"
        );
        drop(presenter);
        assert_eq!(
            Arc::strong_count(&probe),
            4,
            "the final scene owner is retired too"
        );
        drop(release);
        assert!(drained(&probe, 1));
    }
}

#[test]
fn recovery_answers_keep_their_payload_when_delivery_or_validation_is_lost() {
    for lost in [false, true] {
        let scene = SceneId(10);
        let mut presenter = presenter();
        let now = Instant::now();
        described(&mut presenter, &[scene], now);
        let frame = snapshot(scene, rect("recovered"));
        let job = recovery(data(&mut presenter, frame, now).expect("snapshot"));
        let done = job.run(&mut Shaper);
        let Completion::Recovered { scenes, .. } = &done else {
            panic!("a recovery result");
        };
        let (state, _) = scenes[0].1.as_ref().expect("recovered scene");
        let SceneEvent::Step { step, .. } = &state.recovery_log()[0] else {
            panic!("the recovered original step");
        };
        let probe = Arc::clone(step);
        assert!(drained(&probe, 2));
        assert_eq!(Arc::strong_count(&probe), 2, "result and probe");
        let release = hold_dropper();
        let owners = Arc::strong_count(&probe);
        if lost {
            let (sender, receiver) = mpsc::channel();
            drop(receiver);
            drop(
                sender
                    .send(done)
                    .expect_err("the recovery recipient is gone"),
            );
        } else {
            presenter.clear(now).expect("clear before the answer");
            let effects = presenter
                .completed(done, ms(now, 20))
                .expect("obsolete recovery");
            assert!(effects.is_empty());
        }
        assert_eq!(
            Arc::strong_count(&probe),
            owners,
            "discarding the answer keeps its log alive"
        );
        assert_eq!(
            presenter.workers(),
            0,
            "answer disposal releases its slot once"
        );
        drop(presenter);
        drop(release);
        assert!(drained(&probe, 1));
    }
}
