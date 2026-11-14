//! The presenter's test fixtures that tests outside its own test files share.

use std::{
    any::Any,
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

use kumochi_protocol::message::{
    ControlToBrain, DataToShell, GestureId, MAX_VISIBLE_SCENES, RunId, SceneDescriptorV1,
    SceneFailure, SceneHeader, SceneId, SceneSnapshotChunk,
};
use kumochi_scene::{
    Record, SceneEvent, SceneView, Step, StopCut,
    budget::Usage,
    deferred::Deferred,
    display::{MeasureError, PaintWork, TextMeasure, TextStyle},
    layout::SurfaceGeometry,
    parse_record,
    text::Bounded,
    theme::Theme,
};

use super::{
    AdmitJob, Broken, Effect, Frames, PrepareJob, Presenter, RecoveryJob, Sample, SceneCache,
    Stage,
    overlay_input::{self, OverlayOwner, RegionId},
};

pub(crate) const GENERATION: u64 = 1;
pub(crate) const RUN: RunId = RunId(5);
pub(crate) const LIVE: SceneId = SceneId(20);
pub(crate) const FRAME: u32 = 40;

/// Stages caches that hold nothing and charge the given bytes, for tests that never paint one.
pub(crate) struct Charged(pub(crate) usize);

impl Stage for Charged {
    fn fixed_bytes(&self) -> usize {
        0
    }

    fn stage(&self, _: &SceneView) -> Box<dyn SceneCache> {
        Box::new(Charged(self.0))
    }
}

impl SceneCache for Charged {
    fn bytes(&self) -> usize {
        self.0
    }
}

/// Every text measures 8 by 10, except one that can't be shaped.
pub(crate) struct Shaper;

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

pub(crate) fn header() -> SceneHeader {
    let source = br#"{"begin":{"v":1,"scene":"s","surface":"overlay","alt":"diagram"}}"#;
    let Ok(Record::Begin(begin)) = parse_record(source) else {
        panic!("a begin record");
    };
    SceneHeader {
        begin,
        layout_version: super::LAYOUT_VERSION,
    }
}

pub(crate) fn step(source: &str) -> Step {
    let Ok(Record::Step(step)) = parse_record(source.as_bytes()) else {
        panic!("a step record");
    };
    step
}

pub(crate) fn rect(id: &str) -> Step {
    step(&format!(
        r#"{{"step":{{"id":"{id}","put":[{{"shape":{{"id":"{id}","kind":"rect","box":[0,0,10,10]}}}}]}}}}"#
    ))
}

pub(crate) fn presenter() -> Presenter {
    kumochi_scene::deferred::start().expect("the dropper starts");
    let surface = SurfaceGeometry::Overlay {
        default_canvas: [1920.0, 1080.0],
    };
    let mut presenter = Presenter::new(surface, Arc::new(Theme::default()), Box::new(Charged(0)));
    presenter.connected(GENERATION);
    presenter
}

/// A connection whose brain described no v1 scene, so live frames apply at once.
pub(crate) fn live() -> (Presenter, Instant) {
    let mut presenter = presenter();
    let now = Instant::now();
    let effects = presenter
        .described(GENERATION, &Bounded::default(), now)
        .expect("empty membership");
    assert!(effects.is_empty());
    (presenter, now)
}

/// A frame as it arrives, admissions left to the caller.
pub(crate) fn arrive(
    presenter: &mut Presenter,
    frame: DataToShell,
    now: Instant,
) -> Result<Vec<Effect>, Broken> {
    presenter.data(GENERATION, Deferred::new(frame, 0), FRAME, now)
}

/// A frame whose admissions run and answer at once, as a commit applied before the lane.
pub(crate) fn data(
    presenter: &mut Presenter,
    frame: DataToShell,
    now: Instant,
) -> Result<Vec<Effect>, Broken> {
    let effects = arrive(presenter, frame, now)?;
    settle(presenter, effects, now)
}

/// Runs each admission among `effects` and answers it at `now`, until none is sent.
#[expect(
    clippy::panic_in_result_fn,
    reason = "a second admission lane is a test failure, not a stream error"
)]
pub(crate) fn settle(
    presenter: &mut Presenter,
    mut effects: Vec<Effect>,
    now: Instant,
) -> Result<Vec<Effect>, Broken> {
    let mut settled = Vec::new();
    loop {
        let mut admission = None;
        for effect in effects {
            match effect {
                Effect::Admit(job) => {
                    assert!(admission.is_none(), "one admission lane");
                    admission = Some(job);
                }
                effect => settled.push(effect),
            }
        }
        let Some(job) = admission else {
            return Ok(settled);
        };
        effects = presenter.completed(job.run(), now)?;
    }
}

pub(crate) fn commit_frame(scene: SceneId, index: u32, step: Step) -> DataToShell {
    DataToShell::SceneStepCommit {
        run: RUN,
        scene,
        index,
        step,
    }
}

pub(crate) fn begin(presenter: &mut Presenter, scene: SceneId, now: Instant) -> Vec<Effect> {
    let frame = DataToShell::SceneBegin {
        scene,
        header: header(),
    };
    data(presenter, frame, now).expect("a new scene")
}

pub(crate) fn commit(
    presenter: &mut Presenter,
    scene: SceneId,
    index: u32,
    step: Step,
    now: Instant,
) -> Result<Vec<Effect>, Broken> {
    commit_as(presenter, RUN, scene, index, step, now)
}

pub(crate) fn commit_as(
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

pub(crate) fn acknowledge(
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

pub(crate) fn credit(effects: &[Effect]) -> u32 {
    effects
        .iter()
        .map(|effect| match effect {
            Effect::Credit(bytes) => *bytes,
            _ => 0,
        })
        .sum()
}

pub(crate) fn controls(effects: &[Effect]) -> Vec<&ControlToBrain> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Control(message) => Some(message),
            _ => None,
        })
        .collect()
}

pub(crate) fn jobs(effects: Vec<Effect>) -> Vec<PrepareJob> {
    effects
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Prepare(job) => Some(job),
            _ => None,
        })
        .collect()
}

pub(crate) fn recovery(effects: Vec<Effect>) -> RecoveryJob {
    let mut found = effects.into_iter().filter_map(|effect| match effect {
        Effect::Recover(job) => Some(job),
        _ => None,
    });
    let job = found.next().expect("one recovery job");
    assert!(found.next().is_none());
    job
}

pub(crate) fn only_job(effects: Vec<Effect>) -> PrepareJob {
    let mut jobs = jobs(effects);
    assert_eq!(jobs.len(), 1);
    jobs.remove(0)
}

pub(crate) fn admissions(effects: Vec<Effect>) -> Vec<AdmitJob> {
    effects
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Admit(job) => Some(job),
            _ => None,
        })
        .collect()
}

pub(crate) fn only_admission(effects: Vec<Effect>) -> AdmitJob {
    let mut admissions = admissions(effects);
    assert_eq!(admissions.len(), 1);
    admissions.remove(0)
}

pub(crate) fn zero_cut() -> StopCut {
    StopCut {
        activated: 0,
        revealed: Bounded::default(),
    }
}

pub(crate) fn failed(scene: SceneId, index: u32, reason: SceneFailure) -> ControlToBrain {
    ControlToBrain::StepFailed {
        run: RUN,
        scene,
        index,
        reason,
        cut: zero_cut(),
    }
}

/// A live scene whose first step is preparing.
pub(crate) fn preparing() -> (Presenter, Instant, PrepareJob) {
    preparing_for(GENERATION, Box::new(Charged(0)))
}

pub(crate) fn preparing_for(
    generation: u64,
    stage: Box<dyn Stage>,
) -> (Presenter, Instant, PrepareJob) {
    let mut presenter = Presenter::new(
        SurfaceGeometry::Overlay {
            default_canvas: [1920.0, 1080.0],
        },
        Arc::new(Theme::default()),
        stage,
    );
    presenter.connected(generation);
    let now = Instant::now();
    assert!(
        presenter
            .described(generation, &Bounded::default(), now)
            .expect("empty membership")
            .is_empty()
    );
    let begun = presenter
        .data(
            generation,
            Deferred::new(
                DataToShell::SceneBegin {
                    scene: LIVE,
                    header: header(),
                },
                0,
            ),
            FRAME,
            now,
        )
        .expect("a new scene");
    settle(&mut presenter, begun, now).expect("scene admitted");
    let committed = presenter
        .data(
            generation,
            Deferred::new(commit_frame(LIVE, 0, rect("a")), 0),
            FRAME,
            now,
        )
        .expect("a matching step");
    let prepared = settle(&mut presenter, committed, now).expect("step admitted");
    let job = only_job(prepared);
    (presenter, now, job)
}

pub(crate) fn described(
    presenter: &mut Presenter,
    scenes: &[SceneId],
    now: Instant,
) -> Vec<Effect> {
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

pub(crate) fn snapshot(scene: SceneId, step: Step) -> DataToShell {
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
pub(crate) fn recovering(steps: Vec<(SceneId, Step)>) -> (Presenter, Instant, RecoveryJob) {
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

pub(crate) fn painter_fixture(stage: Box<dyn Stage>) -> (Presenter, SceneId) {
    let drawing = step(
        r#"{"step":{"id":"draw","put":[{"shape":{"id":"triangle","kind":"polygon","points":[[0,0],[20,0],[0,20]],"fill":"blue"}},{"line":{"id":"edge","from":[30,0],"via":[[40,20]],"to":[50,0],"width":2,"dash":true}}]}}"#,
    );
    recovered_painter_fixture(stage, drawing)
}

pub(crate) fn table_painter_fixture(stage: Box<dyn Stage>) -> (Presenter, SceneId) {
    use kumochi_scene::display::Prim;

    let (presenter, scene) = recovered_painter_fixture(
        stage,
        step(
            r#"{"step":{"id":"table","put":[{"table":{"id":"t","box":[0,0,100,80],"cols":["A"]}}]}}"#,
        ),
    );
    let mut order = Vec::new();
    assert!(!presenter.frames.borrow().paint(|frame, _| {
        for (_, first, prims) in frame.paint_items() {
            assert_eq!(first, 0);
            order.push(
                prims
                    .iter()
                    .map(|prim| match prim {
                        Prim::Rect { .. } => 0,
                        Prim::Text { .. } => 1,
                        Prim::Line { .. } => 2,
                        Prim::Fill { .. } => 3,
                    })
                    .collect::<Vec<_>>(),
            );
        }
        Ok::<_, ()>(())
    }));
    assert_eq!(
        order,
        [vec![0, 1, 2]],
        "the real frame retains the table header's order"
    );
    (presenter, scene)
}

fn recovered_painter_fixture(stage: Box<dyn Stage>, drawing: Step) -> (Presenter, SceneId) {
    let (mut presenter, now, job) = recovering(vec![(LIVE, drawing)]);
    presenter.stage = stage;
    assert!(
        presenter
            .completed(job.run(&mut Shaper), now)
            .expect("a synthetic recovered drawing")
            .is_empty()
    );
    let (sample, effects) = presenter.sample(0);
    assert!(sample.visible);
    assert!(effects.is_empty());
    (presenter, LIVE)
}

/// Blocks the dropper until the returned sender drops, so whatever is discarded meanwhile stays
/// alive in its queue instead of being destroyed where it was discarded.
pub(crate) fn hold_dropper() -> mpsc::Sender<()> {
    struct Gate(mpsc::Receiver<()>);
    impl Drop for Gate {
        fn drop(&mut self) {
            let _released = self.0.recv();
        }
    }
    kumochi_scene::deferred::start().expect("the dropper starts");
    let (release, gate) = mpsc::channel();
    drop(Deferred::new(Gate(gate), 0));
    release
}

/// Whether the dropper brings `probe` down to `owners` references within a few seconds.
pub(crate) fn drained<T>(probe: &Arc<T>, owners: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Arc::strong_count(probe) > owners {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    true
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "whether the first host survives, as the recovery episode decides"
)]
pub(crate) fn host_recovery_fixture(stage: Box<dyn Stage>, survivor: bool) -> Presenter {
    let mut presenter = Presenter::new(
        SurfaceGeometry::Overlay {
            default_canvas: [1920.0, 1080.0],
        },
        Arc::new(Theme {
            font_size: 15.03125,
            ..Theme::default()
        }),
        stage,
    );
    presenter.connected(GENERATION);
    let now = Instant::now();
    let ids: &[SceneId] = if survivor {
        &[LIVE, SceneId(21)]
    } else {
        &[LIVE]
    };
    assert!(described(&mut presenter, ids, now).is_empty());
    let mut effects = data(&mut presenter, snapshot(LIVE, step(
        r#"{"step":{"id":"table","put":[{"table":{"id":"t","box":[0,0,100,80],"cols":["A"]}}]}}"#,
    )), now).expect("the recovered table chunk");
    if survivor {
        effects.extend(data(&mut presenter, snapshot(SceneId(21), step(
            r#"{"step":{"id":"rect","put":[{"shape":{"id":"r","kind":"rect","box":[120,0,20,20],"fill":"blue"}}]}}"#,
        )), now).expect("the recovered survivor chunk"));
    }
    assert_eq!(
        credit(&effects),
        FRAME * u32::try_from(ids.len()).expect("one or two scenes")
    );
    assert!(
        presenter
            .completed(recovery(effects).run(&mut Shaper), now)
            .expect("the recovered batch")
            .is_empty()
    );
    let (sample, effects) = presenter.sample(0);
    assert!(sample.visible);
    assert!(effects.is_empty());
    assert!(!presenter.frames.borrow().shown);
    assert_eq!(presenter.workers(), 0);
    presenter
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostCharges {
    pub(crate) semantic: usize,
    pub(crate) rest: usize,
    pub(crate) view: usize,
    pub(crate) frame: usize,
    pub(crate) cache: usize,
    pub(crate) activation: usize,
}

impl HostCharges {
    pub(crate) fn prepared(self) -> usize {
        self.rest + self.view + self.frame + self.cache
    }
}

pub(crate) fn host_ledger(
    presenter: &Presenter,
    ids: &[SceneId],
    generation: Option<u64>,
) -> [HostCharges; 2] {
    assert_eq!(presenter.generation, generation);
    assert!(matches!(
        (&presenter.phase, generation),
        (super::Phase::Live, Some(1)) | (super::Phase::Offline, None)
    ));
    assert_eq!(presenter.workers(), 0);
    assert!(presenter.lane.is_none());
    assert_eq!(
        (
            presenter.held.len(),
            presenter.held.capacity(),
            presenter.held_bytes
        ),
        (0, 0, 0)
    );
    assert_eq!(
        presenter
            .returned
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert_eq!(presenter.scenes.len(), ids.len());
    assert!(presenter.scenes.keys().copied().eq(ids.iter().copied()));
    let mut charges = [HostCharges::default(); 2];
    for (index, scene) in presenter.scenes.values().enumerate() {
        assert_eq!(
            (
                scene.queue.len(),
                scene.queue.capacity(),
                scene.ready.len(),
                scene.ready.capacity()
            ),
            (0, 0, 0, 0)
        );
        assert!(scene.running.is_none() && scene.pending_consumption.is_none());
        assert_eq!(
            (
                scene.state.view().revision(),
                scene.state.view().committed(),
                scene.state.view().activated(),
                scene.state.view().object_count()
            ),
            (1, 1, 1, 1)
        );
        let rest = scene.rest.resident_bytes();
        let view = scene.view.resident_bytes();
        assert_eq!((scene.retained, scene.view_bytes), (rest, view));
        charges[index] = HostCharges {
            semantic: 2 * size_of::<usize>()
                + size_of::<kumochi_scene::SceneState>()
                + scene.state.view().attributed_bytes(),
            rest,
            view,
            frame: scene.frame_bytes,
            cache: scene.cache_bytes,
            activation: super::Activation::BYTES,
        };
        assert_eq!(scene.state.attributed_bytes(), charges[index].semantic);
    }
    assert!(
        presenter
            .frames
            .borrow()
            .ids()
            .into_iter()
            .flatten()
            .eq(ids.iter().copied())
    );
    assert_eq!(
        presenter.global.usage(),
        kumochi_scene::budget::Usage {
            objects: ids.len(),
            segments: usize::from(ids.contains(&LIVE)) * 7
                + usize::from(ids.contains(&SceneId(21))) * 29,
            scenes: ids.len(),
            ..kumochi_scene::budget::Usage::default()
        }
    );
    charges
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "whether the episode's link is still connected"
)]
pub(crate) fn host_stale_begin(presenter: &mut Presenter, connected: bool) {
    let effects = presenter
        .data(
            GENERATION,
            Deferred::new(
                DataToShell::SceneBegin {
                    scene: LIVE,
                    header: header(),
                },
                0,
            ),
            FRAME,
            Instant::now(),
        )
        .expect("one late cleared-scene begin");
    assert_eq!(credit(&effects), if connected { FRAME } else { 0 });
    assert!(controls(&effects).is_empty());
    assert!(jobs(effects).is_empty());
    assert!(!presenter.scenes.contains_key(&LIVE));
}

/// Read-ahead positions per scene: prepared results not yet consumed, plus the running one.
pub(crate) const WINDOW: usize = super::scene::WINDOW;

/// The scenes the presenter holds.
pub(crate) fn scene_ids(presenter: &Presenter) -> Vec<SceneId> {
    presenter.scenes.keys().copied().collect()
}

/// How many prepared steps `scene` holds, and how many tickets wait behind them.
pub(crate) fn held(presenter: &Presenter, scene: SceneId) -> (usize, usize) {
    let scene = &presenter.scenes[&scene];
    (scene.ready.len(), scene.queue.len())
}

/// The index of `scene`'s oldest prepared step.
pub(crate) fn ready_head(presenter: &Presenter, scene: SceneId) -> u32 {
    presenter.scenes[&scene]
        .ready
        .front()
        .expect("a ready head")
        .step
        .index
}

/// Whether `scene` has nothing running and nothing waiting.
pub(crate) fn idle(presenter: &Presenter, scene: SceneId) -> bool {
    let scene = &presenter.scenes[&scene];
    scene.running.is_none() && scene.queue.is_empty()
}

/// The revision of the layout `scene`'s next step prepares on.
pub(crate) fn tip_revision(presenter: &Presenter, scene: SceneId) -> u32 {
    presenter.scenes[&scene].tip().revision()
}

/// The objects in `scene`'s committed view.
pub(crate) fn object_count(presenter: &Presenter, scene: SceneId) -> usize {
    presenter.scenes[&scene].state.view().object_count()
}

/// The steps `scene`'s committed view has activated.
pub(crate) fn activated(presenter: &Presenter, scene: SceneId) -> u32 {
    presenter.scenes[&scene].state.view().activated()
}

/// The source of the first step `scene` logged for recovery.
pub(crate) fn first_logged_step(presenter: &Presenter, scene: SceneId) -> Arc<Step> {
    let SceneEvent::Step { step, .. } = &presenter.scenes[&scene].state.view().recovery_log()[0]
    else {
        panic!("the committed source");
    };
    Arc::clone(step)
}

/// What the scenes use of the one shell budget.
pub(crate) fn usage(presenter: &Presenter) -> Usage {
    presenter.global.usage()
}

/// The worker slots answers still hold.
pub(crate) fn workers(presenter: &Presenter) -> usize {
    presenter.workers()
}

/// Shows `scene` at its rest with a fresh cache, as consumption will once it installs views.
pub(crate) fn show_rest(presenter: &mut Presenter, scene: SceneId) {
    presenter
        .frames
        .borrow_mut()
        .replaced(scene, Box::new(Charged(0)));
    let scene = presenter.scenes.get_mut(&scene).expect("scene");
    scene.view = SceneView::new(&scene.rest, scene.state.view().activated());
    scene.view_bytes = scene.view.resident_bytes();
}

/// What the presenter charges against the prepared allowance.
pub(crate) fn residency(presenter: &Presenter) -> usize {
    presenter.residency()
}

/// The paint work the presenter's scenes and retired frames hold.
pub(crate) fn paint_work(presenter: &Presenter) -> PaintWork {
    presenter.paint_work()
}

/// The charge consumed steps their owners still hold.
pub(crate) fn returned(presenter: &Presenter) -> usize {
    presenter.returned.load(Ordering::Relaxed)
}

/// Samples every scene's view at `now_ms`.
pub(crate) fn sample(presenter: &mut Presenter, now_ms: u64) -> (Sample, Vec<Effect>) {
    presenter.sample(now_ms)
}

/// The epoch and bound a shown overlay painted for `scene`.
pub(crate) fn painted(frames: &Frames, scene: SceneId) -> Option<(u64, u32)> {
    frames
        .painted(scene)
        .map(|painted| (painted.epoch, painted.bound))
}

/// Starts the selection for `gesture`.
pub(crate) fn select(owner: &mut OverlayOwner, gesture: GestureId) -> Vec<overlay_input::Effect> {
    owner.select(gesture)
}

/// The region `id` names.
pub(crate) fn region(id: u64) -> RegionId {
    RegionId(id)
}

/// A step consumed through [`consume_ready`], charged until `step` drops.
pub(crate) struct Consumption {
    pub(crate) step: Box<dyn Any>,
    pub(crate) index: u32,
    pub(crate) effects: Result<Vec<Effect>, Broken>,
}

/// Activates `scene`'s oldest prepared step, which must be `index`, as the activation owner will.
pub(crate) fn consume_ready(
    presenter: &mut Presenter,
    scene: SceneId,
    index: u32,
    now: Instant,
    scene_time: u64,
    segment_ms: u64,
) -> Result<Consumption, String> {
    let consumed = presenter
        .consume_ready(scene, index, now, scene_time, segment_ms)
        .map_err(|error| format!("{error:?}"))?;
    Ok(Consumption {
        index: consumed.step.step().index,
        step: Box::new(consumed.step),
        effects: consumed.effects,
    })
}
