use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use kumochi_protocol::message::{
    ControlToBrain, DataToShell, MAX_VISIBLE_SCENES, RunId, SceneCut, SceneCuts, SceneDescriptorV1,
    SceneFailure, SceneHeader, SceneId, SceneSnapshotChunk,
};
use kumochi_scene::{
    Handle, Record, SceneEvent, Step, StopCut,
    display::{MeasureError, TextMeasure, TextStyle},
    layout::SurfaceGeometry,
    parse_record,
    text::Bounded,
    theme::Theme,
};

use super::{Broken, Completion, Effect, PrepareJob, Presenter, RecoveryJob};

const GENERATION: u64 = 1;
const RUN: RunId = RunId(5);
const LIVE: SceneId = SceneId(20);
const FRAME: u32 = 40;

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
            };
            let bytes = u32::try_from(
                frame::encode(&message, Pipe::Data)
                    .expect("bounded typed frame")
                    .len(),
            )
            .expect("frame bytes");
            let start = Instant::now();
            let effects = mirror
                .data(GENERATION, message, bytes, now)
                .expect("legal mirrored update");
            let elapsed = start.elapsed().as_nanos();
            assert_eq!(mirror.scenes[&LIVE].state.object_count(), objects);
            assert_eq!(mirror.scenes[&LIVE].state.revision(), index + 1);
            drop(effects);
            if iteration >= 5 {
                samples.push(elapsed);
            }
        }
        samples.sort_unstable();
        println!(
            "v1_mirror_admission workload={workload} samples=64 warmups=5 p50_ns={} p95_ns={} max_ns={}; decoded frame, pending predecessor, no preparation or painting",
            samples[31], samples[60], samples[63]
        );
    }
}

/// Every text measures 8 by 10, except one that can't be shaped.
struct Shaper;

impl TextMeasure for Shaper {
    fn measure(
        &mut self,
        text: &str,
        _style: TextStyle,
        _width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError> {
        if text == "unshapeable" {
            Err(MeasureError)
        } else {
            Ok([8.0, 10.0])
        }
    }
}

fn header() -> SceneHeader {
    let source = br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":"diagram"}}"#;
    let Ok(Record::Begin(begin)) = parse_record(source) else {
        panic!("a begin record");
    };
    SceneHeader {
        begin,
        layout_version: 1,
    }
}

fn step(source: &str) -> Step {
    let Ok(Record::Step(step)) = parse_record(source.as_bytes()) else {
        panic!("a step record");
    };
    step
}

fn rect(id: &str) -> Step {
    step(&format!(
        r#"{{"step":{{"id":"{id}","put":[{{"shape":{{"id":"{id}","kind":"rect","box":[0,0,10,10]}}}}]}}}}"#
    ))
}

fn label(id: &str, text: &str) -> Step {
    step(&format!(
        r#"{{"step":{{"id":"{id}","put":[{{"text":{{"id":"{id}","at":[10,10],"text":"{text}"}}}}]}}}}"#
    ))
}

fn presenter() -> Presenter {
    let surface = SurfaceGeometry::Overlay {
        logical_size: [1920.0, 1080.0],
    };
    let mut presenter = Presenter::new(surface, Arc::new(Theme::default()));
    presenter.connected(GENERATION);
    presenter
}

/// A connection whose brain described no v1 scene, so live frames apply at once.
fn live() -> (Presenter, Instant) {
    let mut presenter = presenter();
    let now = Instant::now();
    let effects = presenter
        .described(GENERATION, &Bounded::default(), now)
        .expect("empty membership");
    assert!(effects.is_empty());
    (presenter, now)
}

fn data(
    presenter: &mut Presenter,
    frame: DataToShell,
    now: Instant,
) -> Result<Vec<Effect>, Broken> {
    presenter.data(GENERATION, frame, FRAME, now)
}

fn begin(presenter: &mut Presenter, scene: SceneId, now: Instant) -> Vec<Effect> {
    let frame = DataToShell::SceneBegin {
        scene,
        header: header(),
    };
    data(presenter, frame, now).expect("a new scene")
}

fn commit(
    presenter: &mut Presenter,
    scene: SceneId,
    index: u32,
    step: Step,
    now: Instant,
) -> Result<Vec<Effect>, Broken> {
    commit_as(presenter, RUN, scene, index, step, now)
}

fn commit_as(
    presenter: &mut Presenter,
    run: RunId,
    scene: SceneId,
    index: u32,
    step: Step,
    now: Instant,
) -> Result<Vec<Effect>, Broken> {
    let frame = DataToShell::SceneStepCommit {
        run,
        scene,
        index,
        step,
    };
    data(presenter, frame, now)
}

fn acknowledge(
    presenter: &mut Presenter,
    scene: SceneId,
    revision: u32,
    cut: StopCut,
    now: Instant,
) -> Result<Vec<Effect>, Broken> {
    let frame = DataToShell::SceneCutApplied {
        scene,
        revision,
        cut,
    };
    data(presenter, frame, now)
}

fn credit(effects: &[Effect]) -> u32 {
    effects
        .iter()
        .map(|effect| match effect {
            Effect::Credit(bytes) => *bytes,
            _ => 0,
        })
        .sum()
}

fn controls(effects: &[Effect]) -> Vec<&ControlToBrain> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Control(message) => Some(message),
            _ => None,
        })
        .collect()
}

fn jobs(effects: Vec<Effect>) -> Vec<PrepareJob> {
    effects
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Prepare(job) => Some(job),
            _ => None,
        })
        .collect()
}

fn recovery(effects: Vec<Effect>) -> RecoveryJob {
    let mut found = effects.into_iter().filter_map(|effect| match effect {
        Effect::Recover(job) => Some(job),
        _ => None,
    });
    let job = found.next().expect("one recovery job");
    assert!(found.next().is_none());
    job
}

fn only_job(effects: Vec<Effect>) -> PrepareJob {
    let mut jobs = jobs(effects);
    assert_eq!(jobs.len(), 1);
    jobs.remove(0)
}

fn zero_cut() -> StopCut {
    StopCut {
        activated: 0,
        revealed: Bounded::default(),
    }
}

fn failed(scene: SceneId, index: u32, reason: SceneFailure) -> ControlToBrain {
    ControlToBrain::StepFailed {
        run: RUN,
        scene,
        index,
        reason,
        cut: zero_cut(),
    }
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

/// A live scene whose first step is preparing.
fn preparing() -> (Presenter, Instant, PrepareJob) {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let job = only_job(commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a matching step"));
    (presenter, now, job)
}

#[test]
fn a_matching_commit_prepares_once_and_its_result_is_charged() {
    let (mut presenter, now) = live();
    assert_eq!(credit(&begin(&mut presenter, LIVE, now)), FRAME);
    let before = presenter.residency();
    let effects = commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a matching step");
    assert_eq!(credit(&effects), FRAME);
    let job = only_job(effects);
    assert_eq!(job.scene(), LIVE);
    assert_eq!(presenter.deadline(), Some(ms(now, 250)));
    let done = job.run(&mut Shaper);
    let Completion::Prepared {
        result: Ok(prepared),
        ..
    } = &done
    else {
        panic!("a prepared step");
    };
    let charged = prepared.resident_bytes;
    let effects = presenter
        .completed(done, ms(now, 10))
        .expect("a current answer");
    assert!(effects.is_empty());
    assert_eq!(presenter.residency(), before + charged);
    assert_eq!(presenter.deadline(), None);
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
    let (mut presenter, now, first) = preparing();
    let effects = commit(&mut presenter, LIVE, 1, rect("b"), now).expect("a matching step");
    assert!(jobs(effects).is_empty());
    let second = only_job(
        presenter
            .completed(first.run(&mut Shaper), ms(now, 5))
            .expect("a current answer"),
    );
    let effects = presenter
        .completed(second.run(&mut Shaper), ms(now, 10))
        .expect("a current answer");
    assert!(controls(&effects).is_empty());
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
fn a_failed_preparation_cuts_its_scene_with_a_content_free_reason() {
    let (mut presenter, now) = live();
    begin(&mut presenter, LIVE, now);
    let job = only_job(
        commit(&mut presenter, LIVE, 0, label("a", "unshapeable"), now).expect("a matching step"),
    );
    let effects = presenter
        .completed(job.run(&mut Shaper), ms(now, 5))
        .expect("an answer");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 0, SceneFailure::TextMeasure)]
    );
}

#[test]
fn an_answer_before_the_deadline_is_published() {
    let (mut presenter, now, job) = preparing();
    let effects = presenter
        .completed(job.run(&mut Shaper), ms(now, 249))
        .expect("an answer");
    assert!(controls(&effects).is_empty());
}

#[test]
fn an_answer_at_the_deadline_fails_its_step_before_the_watchdog_runs() {
    let (mut presenter, now, job) = preparing();
    let before = presenter.residency();
    let effects = presenter
        .completed(job.run(&mut Shaper), ms(now, 250))
        .expect("an answer");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 0, SceneFailure::Timeout)]
    );
    assert_eq!(presenter.residency(), before);
}

#[test]
fn the_watchdog_fails_a_step_whose_worker_never_answers_and_drops_its_late_answer() {
    let (mut presenter, now, job) = preparing();
    assert!(presenter.tick(ms(now, 249)).expect("a tick").is_empty());
    let effects = presenter.tick(ms(now, 250)).expect("a tick");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 0, SceneFailure::Timeout)]
    );
    assert!(job.cancelled());
    assert_eq!(presenter.deadline(), None);
    let late = presenter
        .completed(job.run(&mut Shaper), ms(now, 400))
        .expect("a late answer");
    assert!(late.is_empty());
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

fn described(presenter: &mut Presenter, scenes: &[SceneId], now: Instant) -> Vec<Effect> {
    let descriptors: Vec<_> = scenes
        .iter()
        .map(|scene| SceneDescriptorV1::new(*scene, 1, 1, 1).expect("counts"))
        .collect();
    let descriptors: Bounded<SceneDescriptorV1, MAX_VISIBLE_SCENES> =
        Bounded::new(descriptors).expect("four scenes");
    presenter
        .described(GENERATION, &descriptors, now)
        .expect("membership")
}

fn snapshot(scene: SceneId, step: Step) -> DataToShell {
    let events = vec![SceneEvent::Step {
        revision: 1,
        step: Arc::new(step),
    }];
    DataToShell::SceneSnapshotV1 {
        chunk: SceneSnapshotChunk {
            scene,
            revision: 1,
            ordinal: 0,
            header: Some(header()),
            events: Bounded::new(events).expect("one event"),
            last: true,
        },
    }
}

/// A connection recovering `scenes`, each with one step; the last snapshot starts the job.
fn recovering(steps: Vec<(SceneId, Step)>) -> (Presenter, Instant, RecoveryJob) {
    let mut presenter = presenter();
    let now = Instant::now();
    let ids: Vec<_> = steps.iter().map(|(scene, _)| *scene).collect();
    assert!(described(&mut presenter, &ids, now).is_empty());
    let mut effects = Vec::new();
    for (scene, step) in steps {
        effects.extend(data(&mut presenter, snapshot(scene, step), now).expect("a chunk"));
    }
    assert_eq!(
        credit(&effects),
        FRAME * u32::try_from(ids.len()).expect("four")
    );
    (presenter, now, recovery(effects))
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

#[test]
fn a_scene_removed_during_recovery_never_installs_and_is_not_cleared_again() {
    let (removed, kept_scene) = (SceneId(10), SceneId(11));
    let (mut presenter, now, job) = recovering(vec![(removed, rect("r")), (kept_scene, rect("k"))]);
    let frame = DataToShell::ScenesRemoved {
        scenes: vec![removed],
    };
    let effects = data(&mut presenter, frame, now).expect("a removal");
    assert_eq!(credit(&effects), FRAME);
    let effects = presenter
        .completed(job.run(&mut Shaper), ms(now, 20))
        .expect("the batch");
    assert!(controls(&effects).is_empty());
    let effects = presenter.stop(RUN, ms(now, 30)).expect("a stop");
    assert_eq!(controls(&effects), [&stop_scenes(vec![kept(kept_scene)])]);
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
fn a_step_that_finds_every_worker_slot_held_fails_at_its_deadline() {
    let (mut presenter, now, _zombies) = exhausted();
    begin(&mut presenter, LIVE, now);
    let effects = commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step");
    assert!(jobs(effects).is_empty());
    assert_eq!(presenter.deadline(), Some(ms(now, 250)));
    let effects = presenter.tick(ms(now, 250)).expect("a tick");
    assert_eq!(
        controls(&effects),
        [&failed(LIVE, 0, SceneFailure::Timeout)]
    );
    assert_eq!(presenter.deadline(), None);
}

#[test]
fn a_freed_worker_slot_starts_the_waiting_step() {
    let (mut presenter, now, mut zombies) = exhausted();
    begin(&mut presenter, LIVE, now);
    commit(&mut presenter, LIVE, 0, rect("a"), now).expect("a step");
    let zombie = zombies.pop().expect("a held slot");
    let effects = presenter
        .completed(zombie.run(&mut Shaper), ms(now, 100))
        .expect("a stale answer");
    assert_eq!(only_job(effects).scene(), LIVE);
    assert_eq!(presenter.deadline(), Some(ms(now, 350)));
}

#[test]
fn a_recovery_that_finds_every_worker_slot_held_clears_its_batch() {
    let (mut presenter, now, _zombies) = exhausted();
    let recovered = SceneId(10);
    presenter.connected(GENERATION + 1);
    let descriptor = SceneDescriptorV1::new(recovered, 1, 1, 1).expect("counts");
    let descriptors = Bounded::new(vec![descriptor]).expect("one scene");
    presenter
        .described(GENERATION + 1, &descriptors, now)
        .expect("membership");
    let effects = presenter
        .data(GENERATION + 1, snapshot(recovered, rect("r")), FRAME, now)
        .expect("a chunk");
    assert_eq!(controls(&effects), [&clear_drawing(vec![recovered])]);
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Recover(_)))
    );
    assert!(!presenter.holds_scenes());
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
    let broken = presenter.completed(job.run(&mut Shaper), ms(now, 20)).err();
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
