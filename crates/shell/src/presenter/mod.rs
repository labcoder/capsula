//! The shell's v1 scene mirror, and the scheduler that prepares its steps off the UI thread.
//!
//! Plain state with no GPUI and no clock: every call takes `now`, returns [`Effect`]s to run in
//! order, and an [`Err`] of [`Broken`] ends the connection. Jobs run on a background executor,
//! and every [`Completion`] comes back through [`Presenter::completed`], stale ones included.
//!
//! - Live frames wait until the connection's recovered batch is installed, so no live suffix
//!   overtakes recovery. Their credit is withheld meanwhile, so the data window bounds them.
//! - Each `StepCommit` goes through the mirror's `admit` and `commit` under one shell budget,
//!   and its lifetime index must match before anything is prepared.
//! - A scene prepares one step at a time, on the tip the previous step left. A result is checked
//!   again when it comes back: job, scene, cut and layout base. A step has 250 ms from its start;
//!   the watchdog fails it then, whether or not its worker has answered.
//! - Nothing activates before B6, so a fresh scene's prepared steps are held and charged, and a
//!   cut or failure names the activated bound: the recovered resting layout, or zero.
//! - A scene with a cut outstanding prepares nothing until the brain acknowledges the cut, and a
//!   scene gets at most one cut between commits.

mod measure;
mod prepare;
mod snapshot;

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use kumochi_protocol::message::{
    ControlToBrain, DATA_WINDOW, DataToShell, MAX_VISIBLE_SCENES, RunId, SceneCut, SceneCuts,
    SceneDescriptorV1, SceneFailure, SceneId,
};
use kumochi_scene::{
    Admission, GlobalBudget, Preparation, SceneState, SceneView, Step, StopCut, StopOutcome,
    layout::{
        LayoutState, PrepareError, PrepareInput, PreparedStep, SceneRecovery, SurfaceGeometry,
    },
    limits::MAX_RESIDENT_BYTES,
    text::Bounded,
    theme::Theme,
};

pub(crate) use measure::Measure;
pub(crate) use prepare::{Completion, PrepareJob, RecoveryJob};
use snapshot::{Assembler, Source};

/// The layout algorithm this shell prepares with; the brain stores it with each scene.
const LAYOUT_VERSION: u32 = 1;
/// A step's preparation should take no longer; missing it is only logged.
const TARGET: Duration = Duration::from_millis(50);
/// A step's preparation fails this long after it started, and a later answer is dropped.
const DEADLINE: Duration = Duration::from_millis(250);
// shortcut: margin over measured core recovery; B9 must measure real-font cap scenes.
const RECOVERY_DEADLINE: Duration = Duration::from_secs(2);
/// Jobs alive at once, cancelled ones included, since cancelling only asks. A job, then its
/// answer, holds a slot until the answer is applied or dropped, spawned or not. Work that finds
/// every slot taken waits at most its own deadline, and a recovery that finds them taken clears
/// its batch.
const MAX_WORKERS: usize = 8;
/// The newest cleared or removed ids remembered, so their late frames are dropped.
const CLEARED_KEPT: usize = 64;
const BOUND: &str = "a mirrored scene is bound to the presenter's budget";

/// What the brain sent can't be mirrored, so the connection ends and recovery starts again.
/// It names the rule, never drawing content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Broken {
    #[error(transparent)]
    Snapshot(#[from] snapshot::Broken),
    #[error("a scene frame out of order")]
    Order,
    #[error("an invalid scene header")]
    Header,
    #[error("more scenes than the cap")]
    Scenes,
    #[error("a committed step the mirror rejects")]
    Step,
    #[error("a step index the mirror disagrees with")]
    Index,
    #[error("a cut the mirror rejects")]
    Cut,
    #[error("held scene frames past the credit window")]
    Window,
}

/// What the caller does, in order.
#[derive(Debug)]
pub(crate) enum Effect {
    /// Grant this much data credit on the current connection.
    Credit(u32),
    /// Queue this on the control pipe.
    Control(ControlToBrain),
    /// Run on a background executor, then pass the answer to [`Presenter::completed`].
    Prepare(PrepareJob),
    /// Run on a background executor, then pass the answer to [`Presenter::completed`].
    Recover(RecoveryJob),
}

/// Every v1 scene the shell mirrors, and the work preparing them.
pub(crate) struct Presenter {
    surface: SurfaceGeometry,
    theme: Arc<Theme>,
    generation: Option<u64>,
    phase: Phase,
    assembler: Assembler,
    /// The one budget every mirrored scene is bound to.
    global: GlobalBudget,
    scenes: BTreeMap<SceneId, Scene>,
    /// Live frames that arrived before recovery was installed, with their withheld credit.
    held: VecDeque<(DataToShell, u32)>,
    held_bytes: u32,
    /// A clear came before recovery was described, so the held live scenes clear once applied.
    clear_after_recovery: bool,
    /// Runs up to this one were stopped: their steps reach the mirror and are never prepared.
    fenced: Option<RunId>,
    /// The newest cleared or removed ids, oldest first.
    // shortcut: tombstones assume one persisted database; add a store id before supporting live database wipes.
    cleared: VecDeque<SceneId>,
    next_job: u64,
    /// One reference per job or answer alive, so dropping either frees its slot.
    slots: Arc<()>,
}

impl Presenter {
    pub(crate) fn new(surface: SurfaceGeometry, theme: Arc<Theme>) -> Self {
        Self {
            surface,
            theme,
            generation: None,
            phase: Phase::Offline,
            assembler: Assembler::default(),
            global: GlobalBudget::default(),
            scenes: BTreeMap::new(),
            held: VecDeque::new(),
            held_bytes: 0,
            clear_after_recovery: false,
            fenced: None,
            cleared: VecDeque::new(),
            next_job: 0,
            slots: Arc::new(()),
        }
    }

    /// A new connection preserves clear intent because stored scene ids survive brain restarts.
    pub(crate) fn connected(&mut self, generation: u64) {
        self.disconnected();
        self.generation = Some(generation);
        self.phase = Phase::Assembling;
        self.assembler.connected(generation);
    }

    /// The connection ended: every job is cancelled and every scene, charge and held frame goes.
    pub(crate) fn disconnected(&mut self) {
        if let Phase::Recovering(recovery) = std::mem::replace(&mut self.phase, Phase::Offline) {
            recovery.cancel.store(true, Ordering::Relaxed);
        }
        for scene in self.scenes.values_mut() {
            scene.halt();
        }
        self.scenes.clear();
        self.global = GlobalBudget::default();
        self.held.clear();
        self.held_bytes = 0;
        self.clear_after_recovery = false;
        self.fenced = None;
        self.generation = None;
        self.assembler.disconnected();
    }

    /// The connection's `ScenesV1`. A legacy `Scenes` means the brain has no v1 scene, so the
    /// caller passes an empty list for it.
    /// # Errors
    /// [`Broken`] for membership the snapshot rules reject.
    pub(crate) fn described(
        &mut self,
        generation: u64,
        scenes: &Bounded<SceneDescriptorV1, MAX_VISIBLE_SCENES>,
        now: Instant,
    ) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        if self.generation == Some(generation) {
            let cleared: Vec<_> = scenes
                .iter()
                .filter(|named| self.cleared.contains(&named.scene))
                .map(|named| named.scene)
                .collect();
            if !cleared.is_empty() {
                let effect = self.assembler.remove(&cleared);
                self.assembled(effect, now, &mut out)?;
            }
            let effect = self.assembler.describe(generation, scenes)?;
            self.assembled(effect, now, &mut out)?;
        }
        Ok(out.finish())
    }

    /// A data frame of size `bytes`, header included. Only snapshot chunks, the v1 live frames
    /// and `ScenesRemoved` are the presenter's; any other frame does nothing and earns no credit.
    /// # Errors
    /// [`Broken`] for a frame the mirror can't apply.
    pub(crate) fn data(
        &mut self,
        generation: u64,
        frame: DataToShell,
        bytes: u32,
        now: Instant,
    ) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        if self.generation != Some(generation) {
            return Ok(out.finish());
        }
        match frame {
            DataToShell::SceneSnapshotV1 { chunk } => {
                let effect = self.assembler.chunk(generation, chunk, bytes)?;
                self.assembled(effect, now, &mut out)?;
            }
            DataToShell::ScenesRemoved { scenes } => {
                out.credit(bytes);
                self.removed(&scenes, now, &mut out)?;
            }
            frame @ (DataToShell::SceneBegin { .. }
            | DataToShell::SceneStepCommit { .. }
            | DataToShell::SceneCutApplied { .. }
            | DataToShell::SceneEnd { .. }) => {
                if matches!(self.phase, Phase::Live) {
                    self.live(frame, bytes, now, &mut out)?;
                } else {
                    self.held_bytes = self
                        .held_bytes
                        .checked_add(bytes)
                        .filter(|held| *held <= DATA_WINDOW)
                        .ok_or(Broken::Window)?;
                    self.held.push_back((frame, bytes));
                }
            }
            _ => {}
        }
        Ok(out.finish())
    }

    /// A job's answer. Answers for cancelled, timed-out or replaced work are dropped, but still
    /// free their worker.
    /// # Errors
    /// [`Broken`] when a held frame applied after recovery can't be mirrored.
    pub(crate) fn completed(
        &mut self,
        done: Completion,
        now: Instant,
    ) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        match done {
            Completion::Prepared {
                id,
                scene,
                result,
                slot,
            } => {
                drop(slot);
                let running = self
                    .scenes
                    .get_mut(&scene)
                    .and_then(|state| state.running.take_if(|running| running.id == id));
                if let Some(running) = running {
                    self.prepared(scene, &running, result, now, &mut out);
                }
            }
            Completion::Recovered {
                id,
                scenes,
                global,
                slot,
            } => {
                drop(slot);
                let late = match &self.phase {
                    Phase::Recovering(recovery) if recovery.id == id => {
                        Some(now >= recovery.started + RECOVERY_DEADLINE)
                    }
                    _ => None,
                };
                match late {
                    Some(true) => self.abandon(now, &mut out)?,
                    Some(false) => {
                        if let Phase::Recovering(recovery) =
                            std::mem::replace(&mut self.phase, Phase::Assembling)
                        {
                            self.recovered(recovery, scenes, global, now, &mut out)?;
                        }
                    }
                    None => {}
                }
            }
        }
        self.pump_all(now, &mut out)?;
        Ok(out.finish())
    }

    /// The watchdog: fails every step past its deadline and abandons a recovery past its own.
    /// Call it when [`Presenter::deadline`] passes, independently of any job.
    /// # Errors
    /// [`Broken`] when a held frame applied after abandoning recovery can't be mirrored.
    pub(crate) fn tick(&mut self, now: Instant) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        let late: Vec<_> = self
            .scenes
            .iter()
            .filter_map(|(scene, state)| {
                let (since, run, index) = state.due()?;
                (now >= since + DEADLINE).then_some((*scene, run, index))
            })
            .collect();
        for (scene, run, index) in late {
            self.fail(scene, run, index, SceneFailure::Timeout, &mut out);
        }
        if matches!(&self.phase, Phase::Recovering(recovery) if now >= recovery.started + RECOVERY_DEADLINE)
        {
            self.abandon(now, &mut out)?;
        }
        self.pump_all(now, &mut out)?;
        Ok(out.finish())
    }

    /// The earliest deadline the watchdog must check, if any work is running.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let recovering = match &self.phase {
            Phase::Recovering(recovery) => Some(recovery.started + RECOVERY_DEADLINE),
            _ => None,
        };
        self.scenes
            .values()
            .filter_map(Scene::due)
            .map(|(since, ..)| since + DEADLINE)
            .chain(recovering)
            .min()
    }

    /// The first Esc. Runs up to `run` are fenced, an incomplete recovery is cleared first, and
    /// the last effect is the `StopScenes` naming every known scene not cut since its last commit.
    /// # Errors
    /// [`Broken`] when a held frame applied after clearing recovery can't be mirrored.
    pub(crate) fn stop(&mut self, run: RunId, now: Instant) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        self.fenced = Some(self.fenced.map_or(run, |fenced| fenced.max(run)));
        if self.recovery_incomplete() {
            self.abandon(now, &mut out)?;
        } else if !matches!(self.phase, Phase::Live) {
            out.credit(self.assembler.clear().credit);
        }
        out.flush();
        let mut cuts = Vec::new();
        for (id, scene) in &mut self.scenes {
            if scene.cut != Cut::Open {
                continue;
            }
            scene.halt();
            scene.skipping = true;
            scene.cut = Cut::Outstanding;
            cuts.push(SceneCut {
                scene: *id,
                cut: scene.cut_now(),
            });
        }
        let cuts = SceneCuts::new(cuts).expect("four scenes within the global caps fit one Stop");
        out.effects
            .push(Effect::Control(ControlToBrain::StopScenes { run, cuts }));
        Ok(out.finish())
    }

    /// The second Esc: every scene goes, recovery included, in one `ClearDrawing`.
    /// # Errors
    /// [`Broken`] when a held frame applied after clearing recovery can't be mirrored.
    pub(crate) fn clear(&mut self, now: Instant) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        if self.recovery_incomplete() {
            self.abandon(now, &mut out)?;
        } else if !matches!(self.phase, Phase::Live) {
            out.credit(self.assembler.clear().credit);
            self.clear_after_recovery = true;
        }
        let ids: Vec<_> = self.scenes.keys().copied().collect();
        for id in ids {
            self.release(id);
            self.forget(id, &mut out);
        }
        Ok(out.finish())
    }

    /// This connection's v1 membership arrived, empty or not.
    pub(crate) fn membership_ready(&self) -> bool {
        self.assembler.described()
    }

    /// Whether a v1 scene is mirrored or recovering, so Esc sends v1 cuts, not the placeholder Stop.
    pub(crate) fn holds_scenes(&self) -> bool {
        !self.scenes.is_empty()
            || matches!(self.phase, Phase::Recovering(_))
            || self.assembler.described() && self.assembler.pending()
    }

    /// Prepared geometry charged against [`MAX_RESIDENT_BYTES`]: what each scene retains plus its
    /// prepared steps.
    pub(crate) fn residency(&self) -> usize {
        self.scenes.values().map(Scene::residency).sum()
    }
}

struct Scene {
    state: SceneState,
    /// The layout at the activated bound: recovered at rest, or empty for a fresh scene.
    rest: LayoutState,
    retained: usize,
    queue: VecDeque<Ticket>,
    running: Option<Running>,
    /// Prepared and not activated; nothing activates before B6.
    ready: Vec<PreparedStep>,
    cut: Cut,
    /// A commit went unprepared, so nothing prepares again until a cut resets the layout.
    skipping: bool,
}

struct Ticket {
    run: RunId,
    index: u32,
    queued: Instant,
    preparation: Preparation,
}

struct Running {
    id: u64,
    run: RunId,
    index: u32,
    started: Instant,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    /// No cut since the last commit.
    Open,
    /// A cut was sent and the brain hasn't acknowledged it.
    Outstanding,
    /// A cut was applied, and no step has committed since.
    Applied,
}

#[derive(Debug)]
enum Phase {
    Offline,
    /// Waiting for `ScenesV1` and every chunk it names.
    Assembling,
    /// The batch is being recovered.
    Recovering(Recovery),
    Live,
}

#[derive(Debug)]
struct Recovery {
    id: u64,
    started: Instant,
    cancel: Arc<AtomicBool>,
    /// Each scene with its descriptor's revision and retained count.
    scenes: Vec<(SceneId, u32, u32)>,
}

/// Effects in order, with the clears gathered into as few `ClearDrawing`s as possible.
#[derive(Default)]
struct Out {
    effects: Vec<Effect>,
    clear: Vec<SceneId>,
}

impl Out {
    fn credit(&mut self, bytes: u32) {
        if bytes > 0 {
            self.effects.push(Effect::Credit(bytes));
        }
    }

    fn flush(&mut self) {
        self.clear.sort_unstable();
        self.clear.dedup();
        for scene_ids in self.clear.chunks(MAX_VISIBLE_SCENES) {
            let scene_ids = scene_ids.to_vec();
            self.effects
                .push(Effect::Control(ControlToBrain::ClearDrawing { scene_ids }));
        }
        self.clear.clear();
    }

    fn finish(mut self) -> Vec<Effect> {
        self.flush();
        self.effects
    }
}

impl Scene {
    fn new(state: SceneState, rest: LayoutState) -> Self {
        Self {
            retained: rest.resident_bytes(),
            state,
            rest,
            queue: VecDeque::new(),
            running: None,
            ready: Vec::new(),
            cut: Cut::Open,
            skipping: false,
        }
    }

    /// The step the watchdog times, and since when: the running one, or else the next queued one,
    /// which only waits while every worker slot is taken.
    fn due(&self) -> Option<(Instant, RunId, u32)> {
        match (&self.running, self.queue.front()) {
            (Some(running), _) => Some((running.started, running.run, running.index)),
            (None, Some(ticket)) => Some((ticket.queued, ticket.run, ticket.index)),
            (None, None) => None,
        }
    }

    /// The layout the next step prepares on.
    fn tip(&self) -> &LayoutState {
        self.ready
            .last()
            .map_or(&self.rest, |prepared| &prepared.after)
    }

    fn residency(&self) -> usize {
        self.ready.iter().fold(self.retained, |sum, prepared| {
            sum.saturating_add(prepared.resident_bytes)
        })
    }

    /// Drops queued and prepared work, and asks a running job to stop.
    fn halt(&mut self) {
        if let Some(running) = self.running.take() {
            running.cancel.store(true, Ordering::Relaxed);
        }
        self.queue.clear();
        self.ready.clear();
    }

    /// The cut at the activated bound, keeping every object at rest there.
    fn cut_now(&self) -> StopCut {
        let activated = self.state.activated();
        let revealed = SceneView::new(&self.rest, activated).stop(0);
        StopCut {
            activated,
            revealed: Bounded::new(revealed).expect("a scene holds at most its object cap"),
        }
    }
}

impl std::fmt::Debug for Presenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presenter")
            .field("generation", &self.generation)
            .field("phase", &self.phase)
            .field("scenes", &self.scenes)
            .field("held", &self.held.len())
            .field("workers", &self.workers())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Scene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scene")
            .field("revision", &self.state.revision())
            .field("queued", &self.queue.len())
            .field("ready", &self.ready.len())
            .field("cut", &self.cut)
            .finish_non_exhaustive()
    }
}

impl Presenter {
    fn assembled(
        &mut self,
        effect: snapshot::Effect,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        out.credit(effect.credit);
        for scene in effect.clear {
            out.clear.push(scene);
            self.remember(scene);
        }
        match effect.batch {
            Some(batch) if matches!(self.phase, Phase::Assembling) => self.recover(batch, now, out),
            _ => Ok(()),
        }
    }

    fn recover(&mut self, batch: Vec<Source>, now: Instant, out: &mut Out) -> Result<(), Broken> {
        let mut sources = Vec::new();
        for source in batch {
            if source.header.layout_version == LAYOUT_VERSION {
                sources.push(source);
            } else {
                self.forget(source.scene, out);
            }
        }
        if self.workers() >= MAX_WORKERS {
            for source in &sources {
                self.forget(source.scene, out);
            }
            sources.clear();
        }
        if sources.is_empty() {
            self.global = GlobalBudget::default();
            return self.go_live(now, out);
        }
        let id = self.job_id();
        let cancel = Arc::new(AtomicBool::new(false));
        let scenes = sources
            .iter()
            .map(|source| (source.scene, source.revision, source.committed))
            .collect();
        let job = RecoveryJob::new(
            id,
            sources,
            self.surface,
            Arc::clone(&self.theme),
            Arc::clone(&cancel),
            Arc::clone(&self.slots),
        );
        out.effects.push(Effect::Recover(job));
        self.phase = Phase::Recovering(Recovery {
            id,
            started: now,
            cancel,
            scenes,
        });
        Ok(())
    }

    fn recovered(
        &mut self,
        recovery: Recovery,
        scenes: Vec<(SceneId, SceneRecovery)>,
        global: GlobalBudget,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        self.global = global;
        let mut results: BTreeMap<_, _> = scenes.into_iter().collect();
        for (scene, revision, committed) in recovery.scenes {
            // shortcut: a recovery failure clears the whole scene instead of cutting to its prepared prefix; add a scene-keyed RecoveryFailed cut if B9's recovery measurements show failures
            match results.remove(&scene) {
                Some(Ok((state, rest)))
                    if !self.cleared.contains(&scene)
                        && state.revision() == revision
                        && usize::try_from(committed)
                            .is_ok_and(|kept| kept == state.committed()) =>
                {
                    self.scenes.insert(scene, Scene::new(state, rest));
                }
                Some(Ok((mut state, _))) => {
                    state.clear(&mut self.global).expect(BOUND);
                    self.forget(scene, out);
                }
                Some(Err(_)) | None => self.forget(scene, out),
            }
        }
        let mut total = 0_usize;
        let over: Vec<_> = self
            .scenes
            .iter()
            .filter_map(|(id, scene)| {
                let next = total.saturating_add(scene.residency());
                if next > MAX_RESIDENT_BYTES {
                    return Some(*id);
                }
                total = next;
                None
            })
            .collect();
        for id in over {
            self.release(id);
            self.forget(id, out);
        }
        self.go_live(now, out)
    }

    /// Recovery that can't finish, or that a Stop or clear overtook: its scenes are cleared, and
    /// the held live frames apply.
    fn abandon(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        let effect = self.assembler.clear();
        out.credit(effect.credit);
        for scene in effect.clear {
            self.forget(scene, out);
        }
        if let Phase::Recovering(recovery) = std::mem::replace(&mut self.phase, Phase::Assembling) {
            recovery.cancel.store(true, Ordering::Relaxed);
            for (scene, ..) in recovery.scenes {
                self.forget(scene, out);
            }
        }
        self.global = GlobalBudget::default();
        self.go_live(now, out)
    }

    /// Recovery is installed: the held live frames apply in arrival order.
    fn go_live(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        self.phase = Phase::Live;
        let held = std::mem::take(&mut self.held);
        self.held_bytes = 0;
        for (frame, bytes) in held {
            self.live(frame, bytes, now, out)?;
        }
        if std::mem::take(&mut self.clear_after_recovery) {
            let ids: Vec<_> = self.scenes.keys().copied().collect();
            for id in ids {
                self.release(id);
                self.forget(id, out);
            }
        }
        Ok(())
    }

    fn recovery_incomplete(&self) -> bool {
        match self.phase {
            Phase::Recovering(_) => true,
            Phase::Assembling => self.assembler.described(),
            Phase::Offline | Phase::Live => false,
        }
    }

    /// The brain removed scenes: they go now, in any phase, and nothing of them installs later.
    fn removed(&mut self, scenes: &[SceneId], now: Instant, out: &mut Out) -> Result<(), Broken> {
        let effect = self.assembler.remove(scenes);
        for scene in scenes {
            self.release(*scene);
            self.remember(*scene);
        }
        out.credit(effect.credit);
        match effect.batch {
            Some(batch) if matches!(self.phase, Phase::Assembling) => self.recover(batch, now, out),
            _ => Ok(()),
        }
    }

    fn live(
        &mut self,
        frame: DataToShell,
        bytes: u32,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        out.credit(bytes);
        match frame {
            DataToShell::SceneBegin { scene, header } => {
                if self.cleared.contains(&scene) {
                    return Ok(());
                }
                if self.scenes.contains_key(&scene) {
                    return Err(Broken::Order);
                }
                if header.layout_version != LAYOUT_VERSION {
                    return Err(Broken::Header);
                }
                if self.scenes.len() >= MAX_VISIBLE_SCENES {
                    return Err(Broken::Scenes);
                }
                let state = SceneState::begin(header.begin).map_err(|_| Broken::Header)?;
                let rest = LayoutState::new(&state);
                self.scenes.insert(scene, Scene::new(state, rest));
                Ok(())
            }
            DataToShell::SceneStepCommit {
                run,
                scene,
                index,
                step,
            } => self.commit(run, scene, index, &step, now, out),
            DataToShell::SceneCutApplied {
                scene,
                revision,
                cut,
            } => self.cut_applied(scene, revision, &cut, out),
            _ => Ok(()),
        }
    }

    fn commit(
        &mut self,
        run: RunId,
        id: SceneId,
        index: u32,
        step: &Step,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        if self.cleared.contains(&id) {
            return Ok(());
        }
        let fenced = self.fenced.is_some_and(|fenced| run <= fenced);
        let scene = self.scenes.get_mut(&id).ok_or(Broken::Order)?;
        if index != scene.state.revision() {
            return Err(Broken::Index);
        }
        let Ok(Admission::New(admitted)) = scene.state.admit(step, &self.global) else {
            return Err(Broken::Step);
        };
        let committed = scene
            .state
            .commit(admitted, &mut self.global)
            .map_err(|_| Broken::Step)?;
        if committed.index != index {
            return Err(Broken::Index);
        }
        let applied = scene.cut == Cut::Applied;
        if applied {
            scene.cut = Cut::Open;
        }
        if scene.cut == Cut::Outstanding || fenced {
            scene.skipping = true;
            // A stopped run's late step on a scene its Stop skipped as already cut: this commit
            // allows one new cut, and only the shell can send it.
            if fenced && applied {
                scene.cut = Cut::Outstanding;
                out.effects
                    .push(Effect::Control(ControlToBrain::StepFailed {
                        run,
                        scene: id,
                        index,
                        reason: SceneFailure::Cancelled,
                        cut: scene.cut_now(),
                    }));
            }
            return Ok(());
        }
        if scene.skipping {
            return Err(Broken::Order);
        }
        scene.queue.push_back(Ticket {
            run,
            index,
            queued: now,
            preparation: committed.preparation,
        });
        self.pump(id, now, out)
    }

    /// The brain applied a cut after every step it committed before it. A scene it no longer
    /// holds is stale, but a cut the mirror rejects means the two disagree.
    fn cut_applied(
        &mut self,
        id: SceneId,
        revision: u32,
        cut: &StopCut,
        out: &mut Out,
    ) -> Result<(), Broken> {
        let Some(scene) = self.scenes.get_mut(&id) else {
            return Ok(());
        };
        if scene.state.revision() != revision {
            return Err(Broken::Cut);
        }
        scene.halt();
        let outcome = scene
            .state
            .stop(cut, &mut self.global)
            .map_err(|_| Broken::Cut)?;
        scene.cut = Cut::Applied;
        scene.skipping = false;
        let rest = match outcome {
            StopOutcome::Kept => scene.rest.cut(&scene.state).ok(),
            StopOutcome::Cleared => None,
        };
        if let Some(rest) = rest {
            scene.retained = rest.resident_bytes();
            scene.rest = rest;
        } else {
            self.release(id);
            if outcome == StopOutcome::Kept {
                self.forget(id, out);
            } else {
                self.remember(id);
            }
        }
        Ok(())
    }

    fn pump_all(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        let ids: Vec<_> = self.scenes.keys().copied().collect();
        for id in ids {
            self.pump(id, now, out)?;
        }
        Ok(())
    }

    /// Starts a scene's next queued step on the tip its previous step left.
    fn pump(&mut self, id: SceneId, now: Instant, out: &mut Out) -> Result<(), Broken> {
        if self.workers() >= MAX_WORKERS {
            return Ok(());
        }
        let job = self.next_job;
        let Some(scene) = self.scenes.get_mut(&id) else {
            return Ok(());
        };
        if scene.running.is_some() || scene.cut == Cut::Outstanding {
            return Ok(());
        }
        let Some(ticket) = scene.queue.pop_front() else {
            return Ok(());
        };
        let (run, index) = (ticket.run, ticket.index);
        let input = PrepareInput::new(
            ticket.preparation,
            scene.tip().clone(),
            self.surface,
            Arc::clone(&self.theme),
        );
        match input {
            Ok(input) => {
                let cancel = Arc::new(AtomicBool::new(false));
                scene.running = Some(Running {
                    id: job,
                    run,
                    index,
                    started: now,
                    cancel: Arc::clone(&cancel),
                });
                self.next_job += 1;
                let slot = Arc::clone(&self.slots);
                out.effects.push(Effect::Prepare(PrepareJob::new(
                    job, id, input, cancel, slot,
                )));
                Ok(())
            }
            Err((PrepareError::Stale, _)) => Err(Broken::Order),
            Err((error, _)) => {
                self.fail(id, run, index, failure(&error), out);
                Ok(())
            }
        }
    }

    /// Publishes a current answer, or fails its step: late, invalid, stale or over residency.
    fn prepared(
        &mut self,
        id: SceneId,
        running: &Running,
        result: Result<PreparedStep, PrepareError>,
        now: Instant,
        out: &mut Out,
    ) {
        let elapsed = now.saturating_duration_since(running.started);
        let total = self.residency();
        let Some(scene) = self.scenes.get_mut(&id) else {
            return;
        };
        let accepted = if elapsed >= DEADLINE {
            Err(SceneFailure::Timeout)
        } else {
            result.map_err(|error| failure(&error))
        }
        .and_then(|prepared| {
            if prepared.index != running.index
                || prepared.validate(&scene.state, scene.tip()).is_err()
            {
                Err(SceneFailure::Validation)
            } else if total.saturating_add(prepared.resident_bytes) > MAX_RESIDENT_BYTES {
                Err(SceneFailure::Residency)
            } else {
                Ok(prepared)
            }
        });
        match accepted {
            Ok(prepared) => {
                if elapsed > TARGET {
                    let elapsed_ms = elapsed.as_millis();
                    tracing::warn!(elapsed_ms, "a scene step's preparation missed its target");
                }
                scene.ready.push(prepared);
            }
            Err(reason) => self.fail(id, running.run, running.index, reason, out),
        }
    }

    /// A step failed: its scene drops its work and, unless already cut since its last commit,
    /// sends the cut at its activated bound and waits for the brain to acknowledge it.
    fn fail(&mut self, id: SceneId, run: RunId, index: u32, reason: SceneFailure, out: &mut Out) {
        let Some(scene) = self.scenes.get_mut(&id) else {
            return;
        };
        scene.halt();
        scene.skipping = true;
        if scene.cut != Cut::Open {
            return;
        }
        scene.cut = Cut::Outstanding;
        out.effects
            .push(Effect::Control(ControlToBrain::StepFailed {
                run,
                scene: id,
                index,
                reason,
                cut: scene.cut_now(),
            }));
    }

    /// Drops a scene and releases its budget, residency and work.
    fn release(&mut self, id: SceneId) {
        if let Some(mut scene) = self.scenes.remove(&id) {
            scene.halt();
            scene.state.clear(&mut self.global).expect(BOUND);
        }
    }

    /// A scene the shell clears: the brain is told unless it is already known as gone.
    fn forget(&mut self, id: SceneId, out: &mut Out) {
        if !self.cleared.contains(&id) {
            out.clear.push(id);
            self.remember(id);
        }
    }

    fn remember(&mut self, id: SceneId) {
        if !self.cleared.contains(&id) {
            self.cleared.push_back(id);
        }
        if self.cleared.len() > CLEARED_KEPT {
            self.cleared.pop_front();
        }
    }

    fn workers(&self) -> usize {
        Arc::strong_count(&self.slots) - 1
    }

    fn job_id(&mut self) -> u64 {
        let id = self.next_job;
        self.next_job += 1;
        id
    }
}

fn failure(error: &PrepareError) -> SceneFailure {
    match error {
        PrepareError::Cancelled => SceneFailure::Cancelled,
        PrepareError::Stale => SceneFailure::Validation,
        PrepareError::TextMeasure => SceneFailure::TextMeasure,
        PrepareError::OverResidency => SceneFailure::Residency,
        PrepareError::Layout(_) => SceneFailure::Layout,
    }
}

#[cfg(test)]
mod tests;
