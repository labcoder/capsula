//! The shell's v1 scene mirror, and the scheduler that prepares its steps off the UI thread.
//!
//! Plain state with no GPUI and no clock: every call takes `now`, returns [`Effect`]s to run in
//! order, and an [`Err`] of [`Broken`] ends the connection. Jobs run on a background executor,
//! and every [`Completion`] comes back through [`Presenter::completed`], stale ones included.
//!
//! - Live frames wait until the connection's recovered batch is installed, so no live suffix
//!   overtakes recovery. Their credit is withheld meanwhile, so the data window bounds them.
//! - One `StepCommit` at a time is admitted on a worker against a read-only snapshot of its
//!   scene, then installed through the scene's binding under the one shell budget; its lifetime
//!   index must match first. Every later scene frame waits behind it in arrival order. An
//!   admission has 250 ms from reaching the lane, slot wait included; past that only its scene is
//!   cleared, as if the shell had cleared it.
//! - A scene prepares one step at a time, on the tip the previous step left, and holds at most
//!   [`scene::WINDOW`] prepared or running steps. A result is checked again when it comes back:
//!   job, scene, cut and layout base. A step has 250 ms from when its predecessor was ready and
//!   the window had room, waiting for a worker slot included; the watchdog fails it then, whether
//!   or not its worker has answered. A step the window holds back is untimed.
//! - Only the oldest prepared step is consumed. One transaction stages its activated view against
//!   the current one, checks the whole post-transition charge, then retires it semantically and
//!   installs its layout as the rest with that view in a new frame epoch; a refusal changes
//!   nothing. A cut or failure names the activated bound: the recovered resting layout, zero, or
//!   the last consumed step's.
//! - A scene with a cut outstanding prepares nothing until the brain acknowledges the cut, and a
//!   scene gets at most one cut between commits.
//! - Scene frames, jobs, answers and scenes are held in [`Deferred`] from the moment the
//!   presenter owns them, so whichever path discards them, they drop on the dropper thread. Once the
//!   dropper has stopped, intake and new work stop with it, and the watchdog fails what waits.

mod activation;
mod admit;
mod conduction;
pub(crate) use conduction::RetirementLane;
pub mod overlay_input;
mod paint;
mod placement;
mod prepare;
mod recovery;
mod scene;
mod screen;
mod snapshot;
mod swap;
mod timing;
mod watch;
mod window;

use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use kumochi_protocol::message::{
    ControlToBrain, DATA_WINDOW, DataToShell, MAX_VISIBLE_SCENES, RunId, SceneCut, SceneCuts,
    SceneDescriptorV1, SceneFailure, SceneId,
};
use kumochi_scene::{
    DetachedCandidate, Frame, GlobalBudget, SceneBinding, SceneRead, SceneState, StepError,
    StopCut, StopOutcome,
    deferred::{self, Deferred},
    display::PaintWork,
    layout::{LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry},
    limits::MAX_RESIDENT_BYTES,
    record::ScreenDisplay,
    text::Bounded,
    theme::Theme,
};

use activation::Activation;
use admit::Lane;
pub use paint::{Frames, Sample, SceneCache, Stage};
pub use prepare::{AdmitJob, Completion, PrepareJob, RecoveryJob};
use recovery::Phase;
use scene::{Cut, Out, Ready, Running, Scene, Ticket};
use screen::{left, on_screen};
use snapshot::Assembler;
pub use timing::{DrawKey, DrawTiming};
use watch::Watch;

use crate::platform::screen_watch::{Changed, Sources};
use crate::runs::RunDisposition;

/// The layout algorithm this shell prepares with; the brain stores it with each scene.
const LAYOUT_VERSION: u32 = 2;
/// A step's preparation should take no longer; missing it is only logged.
const TARGET: Duration = Duration::from_millis(50);
/// An admission fails this long after it reached the lane, and a step's preparation this long
/// after it became eligible; a later answer is dropped.
const DEADLINE: Duration = Duration::from_millis(250);
// shortcut: margin over measured core recovery; B9 must measure real-font cap scenes.
const RECOVERY_DEADLINE: Duration = Duration::from_secs(2);
/// Jobs alive at once, cancelled ones included, since cancelling only asks. A job, then its
/// answer, holds a slot until the answer is applied or dropped, spawned or not. Work that finds
/// every slot taken waits at most its own deadline; a recovery batch waits under its own.
const MAX_WORKERS: usize = 8;
/// The newest cleared or removed ids remembered, so their late frames are dropped.
const CLEARED_KEPT: usize = 64;
const BOUND: &str = "a mirrored scene is bound to the presenter's budget";

/// What the brain sent can't be mirrored, so the connection ends and recovery starts again.
/// It names the rule, never drawing content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Broken {
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
pub enum Effect {
    /// Grant this much data credit on the current connection.
    Credit(u32),
    /// Queue this on the control pipe.
    Control(ControlToBrain),
    /// Run on a background executor, then pass the answer to [`Presenter::completed`].
    Prepare(PrepareJob),
    /// Run on a background executor, then pass the answer to [`Presenter::completed`].
    Recover(RecoveryJob),
    /// Run on a background executor, then pass the answer to [`Presenter::completed`].
    Admit(AdmitJob),
    /// A consumption refused as busy can be retried now, before this scene admits again.
    ConsumptionReady { scene: SceneId, index: u32 },
}

/// Every v1 scene the shell mirrors, and the work preparing them.
pub struct Presenter {
    draw_timing: Option<Box<dyn FnMut(DrawTiming)>>,
    conduction: Option<Box<conduction::Conduction>>,
    pub input: overlay_input::OverlayOwner,
    surface: SurfaceGeometry,
    drawing_stage: Option<kumochi_scene::geom::Rect>,
    theme: Arc<Theme>,
    generation: Option<u64>,
    phase: Phase,
    assembler: Assembler,
    /// The one budget every mirrored scene is bound to.
    global: GlobalBudget,
    scenes: BTreeMap<SceneId, Scene>,
    /// Each scene's sampled frame, shared with the overlay that paints it.
    /// Borrow only for paint; never call the presenter while that borrow is held.
    frames: Rc<RefCell<Frames>>,
    /// The one commit being admitted; later scene frames wait in `held` behind it.
    lane: Option<Lane>,
    /// Live frames waiting for recovery or the admission lane, with their withheld credit.
    held: VecDeque<(Deferred<DataToShell>, u32, RunDisposition)>,
    held_bytes: u32,
    /// A clear came before recovery was described, so the held live scenes clear once applied.
    clear_after_recovery: bool,
    /// Runs up to this one were stopped: their steps reach the mirror and are never prepared.
    fenced: Option<RunId>,
    stopped_begins: [Option<SceneId>; MAX_VISIBLE_SCENES],
    /// The newest cleared or removed ids, oldest first.
    // shortcut: tombstones assume one persisted database; add a store id before supporting live database wipes.
    cleared: VecDeque<SceneId>,
    next_job: u64,
    /// A `StepReady` for each step prepared since the last [`Presenter::take_ready`].
    ready: Vec<ControlToBrain>,
    /// One reference per job or answer alive, so dropping either frees its slot.
    slots: Arc<()>,
    /// The dropper stopped and intake was refused; logged once.
    refused: bool,
    /// Consumed steps their owners still hold, charged until each is dropped, across connections.
    returned: Arc<AtomicUsize>,
    /// Stages each committed view's paint cache.
    stage: Box<dyn Stage>,
    /// When a screen scene may be shown, and when it goes.
    watch: Watch,
    /// The platform's sources, started while the watch listens.
    sources: Option<Box<dyn Sources>>,
    /// The bit those sources set.
    changed: Changed,
    listening: bool,
    /// The display the overlay covers, where a screen scene's view is placed.
    display: Option<ScreenDisplay>,
    /// This presenter's dropper liveness in tests; `None` asks the process-wide dropper.
    #[cfg(any(test, feature = "test-support"))]
    dropper: Option<bool>,
}

impl Presenter {
    pub fn new(surface: SurfaceGeometry, theme: Arc<Theme>, stage: Box<dyn Stage>) -> Self {
        Self {
            draw_timing: None,
            conduction: None,
            input: overlay_input::OverlayOwner::default(),
            surface,
            drawing_stage: None,
            theme,
            generation: None,
            phase: Phase::Offline,
            assembler: Assembler::default(),
            global: GlobalBudget::default(),
            scenes: BTreeMap::new(),
            frames: Rc::default(),
            lane: None,
            held: VecDeque::new(),
            held_bytes: 0,
            clear_after_recovery: false,
            fenced: None,
            stopped_begins: [None; MAX_VISIBLE_SCENES],
            cleared: VecDeque::new(),
            next_job: 0,
            ready: Vec::new(),
            slots: Arc::new(()),
            refused: false,
            returned: Arc::new(AtomicUsize::new(0)),
            stage,
            watch: Watch::default(),
            sources: None,
            changed: Changed::default(),
            listening: false,
            display: None,
            #[cfg(any(test, feature = "test-support"))]
            dropper: None,
        }
    }

    /// Installs a startup-only observer of accepted step transitions; it never schedules work.
    pub fn observe_draw_timing(&mut self, observe: impl FnMut(DrawTiming) + 'static) {
        self.draw_timing = Some(Box::new(observe));
    }

    /// A new connection preserves clear intent because stored scene ids survive brain restarts.
    pub fn connected(&mut self, generation: u64) {
        self.disconnected();
        self.generation = Some(generation);
        self.phase = Phase::Assembling;
        self.assembler.connected(generation);
    }

    /// The connection ended: every job is cancelled and every scene, charge and held frame goes.
    pub fn disconnected(&mut self) {
        self.conduction = None;
        self.ready.clear();
        self.discard_replacement();
        if let Phase::Recovering(recovery) = std::mem::replace(&mut self.phase, Phase::Offline) {
            recovery.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(lane) = self.lane.take() {
            lane.cancel();
        }
        for scene in self.scenes.values_mut() {
            scene.halt();
        }
        let bytes = self
            .scenes
            .values()
            .map(|scene| scene.residency() + scene.state.attributed_bytes())
            .sum();
        let frames = self.frames.borrow_mut().take_all();
        drop(Deferred::new(
            (std::mem::take(&mut self.scenes), frames),
            bytes,
        ));
        self.global = GlobalBudget::default();
        drop(Deferred::new(
            std::mem::take(&mut self.held),
            wire_bytes(self.held_bytes),
        ));
        self.held_bytes = 0;
        self.clear_after_recovery = false;
        self.fenced = None;
        self.stopped_begins = [None; MAX_VISIBLE_SCENES];
        self.generation = None;
        self.assembler.disconnected();
        if std::mem::take(&mut self.watch).shown().is_some() {
            left();
        }
        self.hush();
    }

    /// The connection's `ScenesV1`. A legacy `Scenes` means the brain has no v1 scene, so the
    /// caller passes an empty list for it.
    /// # Errors
    /// [`Broken`] for membership the snapshot rules reject.
    pub fn described(
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
    /// and `ScenesRemoved` are the presenter's; any other frame does nothing and earns no credit,
    /// and so does every frame once the dropper has stopped. A stale frame is still the
    /// presenter's to drop.
    /// # Errors
    /// [`Broken`] for a frame the mirror can't apply.
    pub fn data(
        &mut self,
        generation: u64,
        frame: Deferred<DataToShell>,
        bytes: u32,
        now: Instant,
    ) -> Result<Vec<Effect>, Broken> {
        self.data_with_disposition(generation, frame, bytes, RunDisposition::Live, now)
    }

    #[expect(clippy::result_large_err, reason = "preserve rejected ownership")]
    pub fn data_with_disposition(
        &mut self,
        generation: u64,
        frame: Deferred<DataToShell>,
        bytes: u32,
        disposition: RunDisposition,
        now: Instant,
    ) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        let current = self.generation == Some(generation) && self.dropper_running();
        if current {
            self.heed(now, &mut out)?;
            self.expire_marks(now, &mut out)?;
        }
        let snapshot = frame
            .map(|frame| match frame {
                DataToShell::SceneSnapshotV1 { chunk } => Ok(chunk),
                frame => Err(frame),
            })
            .transpose();
        match snapshot {
            Ok(chunk) if current => {
                let effect = self.assembler.chunk(generation, chunk, bytes)?;
                self.assembled(effect, now, &mut out)?;
            }
            Err(frame) if current => {
                self.judge(&frame, now, &mut out)?;
                let removed = matches!(
                    &*frame,
                    DataToShell::ScenesRemoved { .. } | DataToShell::ScenesReplaced { .. }
                );
                let scene = removed
                    || matches!(
                        &*frame,
                        DataToShell::SceneBegin { .. }
                            | DataToShell::SceneStepCommit { .. }
                            | DataToShell::SceneCutApplied { .. }
                            | DataToShell::SceneEnd { .. }
                    );
                let direct = if matches!(self.phase, Phase::Live) {
                    self.lane.is_none()
                        && self.held.is_empty()
                        && !self.waits_for_consumption(&frame)
                } else {
                    removed
                };
                if scene && direct {
                    self.live(frame, bytes, disposition, now, &mut out)?;
                } else if scene {
                    let lane = self.lane.as_ref().map_or(0, |lane| lane.bytes);
                    self.held_bytes = self
                        .held_bytes
                        .checked_add(bytes)
                        .filter(|held| held.saturating_add(lane) <= DATA_WINDOW)
                        .ok_or(Broken::Window)?;
                    self.held.push_back((frame, bytes, disposition));
                }
            }
            Ok(_) | Err(_) => {}
        }
        Ok(out.finish())
    }

    /// A job's answer. Answers for cancelled, timed-out or replaced work are dropped, but still
    /// free their worker. The screen watch is applied first, so the answer of a screen scene
    /// that a change or a limit takes here finds it gone.
    /// # Errors
    /// [`Broken`] when a held frame applied after recovery can't be mirrored.
    pub fn completed(&mut self, done: Completion, now: Instant) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        self.check_marks(now, &mut out)?;
        match done {
            Completion::Prepared {
                id,
                scene,
                result,
                full_after,
                slot,
            } => {
                drop(slot);
                let running = self
                    .scenes
                    .get_mut(&scene)
                    .and_then(|state| state.running.take_if(|running| running.id == id));
                if let Some(running) = running {
                    self.prepared(scene, &running, result, full_after, now, &mut out)?;
                }
            }
            Completion::Admitted {
                id,
                read,
                result,
                slot,
            } => {
                drop(slot);
                self.admitted(id, read, result, now, &mut out)?;
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

    /// The watchdog: fails every step and admission past its deadline and abandons a recovery
    /// past its own.
    /// Call it when [`Presenter::deadline`] passes, independently of any job.
    /// # Errors
    /// [`Broken`] when a held frame applied after abandoning recovery can't be mirrored.
    pub fn tick(&mut self, now: Instant) -> Result<Vec<Effect>, Broken> {
        self.frames.borrow_mut().expire_retirement(now);
        let mut out = Out::default();
        self.heed(now, &mut out)?;
        self.expire_marks(now, &mut out)?;
        let late: Vec<_> = self
            .scenes
            .iter()
            .filter_map(|(scene, state)| {
                let (since, run, index) = state.due()?;
                (now >= since + DEADLINE).then_some((*scene, run, index))
            })
            .collect();
        for (scene, run, index) in late {
            self.fail(scene, run, index, SceneFailure::Timeout, now, &mut out)?;
        }
        if matches!(&self.phase, Phase::Recovering(recovery) if now >= recovery.started + RECOVERY_DEADLINE)
        {
            self.abandon(now, &mut out)?;
        }
        self.pump_all(now, &mut out)?;
        Ok(out.finish())
    }

    /// The earliest deadline the watchdog must check, if any work is timed.
    pub fn deadline(&self) -> Option<Instant> {
        let recovering = match &self.phase {
            Phase::Recovering(recovery) => Some(recovery.started + RECOVERY_DEADLINE),
            _ => None,
        };
        self.scenes
            .values()
            .filter_map(Scene::due)
            .map(|(since, ..)| since + DEADLINE)
            .chain(self.lane.as_ref().map(Lane::deadline))
            .chain(recovering)
            .chain(self.frames.borrow().retirement.as_ref().map(|r| r.expires))
            .chain(self.watch.deadline())
            .min()
    }

    fn stop_membership(&self) -> Result<[Option<SceneId>; MAX_VISIBLE_SCENES], Broken> {
        let mut visible = [None; MAX_VISIBLE_SCENES];
        let mut pending = self
            .frames
            .borrow()
            .successor()
            .filter(|id| !self.scenes.contains_key(id));
        for (slot, id) in visible.iter_mut().zip(self.scenes.keys()) {
            *slot = Some(*id);
        }
        for (frame, _, _) in &self.held {
            match &**frame {
                DataToShell::SceneBegin { scene, .. } if !self.cleared.contains(scene) => {
                    if pending == Some(*scene) {
                        pending = None;
                    }
                    if visible.contains(&Some(*scene)) {
                        return Err(Broken::Order);
                    }
                    let slot = visible
                        .iter_mut()
                        .find(|slot| slot.is_none())
                        .ok_or(Broken::Scenes)?;
                    *slot = Some(*scene);
                }
                DataToShell::ScenesRemoved { scenes }
                | DataToShell::ScenesReplaced { scenes, .. } => {
                    if pending.is_some_and(|id| scenes.contains(&id)) {
                        pending = None;
                    }
                    for slot in &mut visible {
                        if slot.is_some_and(|id| scenes.contains(&id)) {
                            *slot = None;
                        }
                    }
                }
                DataToShell::SceneCutApplied { scene, cut, .. } if cut.activated == 0 => {
                    for slot in &mut visible {
                        if *slot == Some(*scene) {
                            *slot = None;
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(id) = pending {
            *visible
                .iter_mut()
                .find(|slot| slot.is_none())
                .ok_or(Broken::Scenes)? = Some(id);
        }
        Ok(visible)
    }

    /// The first Esc. Runs up to `run` are fenced, an incomplete recovery is cleared first, and
    /// the last effect is the `StopScenes` naming every known scene not cut since its last commit.
    /// # Errors
    /// [`Broken`] when a held frame applied after clearing recovery can't be mirrored.
    pub fn stop(&mut self, run: RunId, now: Instant) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        let run = self.fenced.map_or(run, |fenced| fenced.max(run));
        self.fenced = Some(run);
        if self.recovery_incomplete() {
            self.abandon(now, &mut out)?;
        } else if !matches!(self.phase, Phase::Live) {
            out.credit(self.assembler.clear().credit);
        }
        out.flush();
        let visible = self.stop_membership()?;
        self.discard_replacement();
        for pending in &mut self.stopped_begins {
            if !visible.contains(pending) {
                *pending = None;
            }
        }
        let mut cuts = Vec::new();
        for (id, scene) in &mut self.scenes {
            if scene.cut != Cut::Open {
                continue;
            }
            scene.halt();
            scene.skipping = true;
            scene.cut = Cut::Outstanding;
            scene.stopped_ms = scene.sampled_ms;
            if visible.contains(&Some(*id)) {
                cuts.push(SceneCut {
                    scene: *id,
                    cut: scene.cut_now(),
                });
            }
        }
        for id in visible.into_iter().flatten() {
            if self.scenes.contains_key(&id) || self.stopped_begins.contains(&Some(id)) {
                continue;
            }
            let pending = self
                .stopped_begins
                .iter_mut()
                .find(|slot| slot.is_none())
                .expect("pending held cuts are a subset of the four visible identities");
            *pending = Some(id);
            cuts.push(SceneCut {
                scene: id,
                cut: StopCut {
                    activated: 0,
                    revealed: Bounded::default(),
                },
            });
        }
        self.drain(now, &mut out)?;
        out.flush();
        let cuts = SceneCuts::new(cuts).expect("four scenes within the global caps fit one Stop");
        out.effects
            .push(Effect::Control(ControlToBrain::StopScenes { run, cuts }));
        Ok(out.finish())
    }

    pub fn stop_owned(&mut self, run: RunId, now: Instant) -> Result<Vec<Effect>, Broken> {
        let mut owned = [None; MAX_VISIBLE_SCENES];
        for (slot, (id, scene)) in owned.iter_mut().zip(&self.scenes) {
            if scene.activation.owned_by(run)
                || scene.running.as_ref().is_some_and(|work| work.run == run)
                || scene.ready.iter().any(|work| work.run == run)
                || scene.queue.iter().any(|work| work.run == run)
            {
                *slot = Some(*id);
            }
        }
        if let Some(lane) = self.lane.as_mut().filter(|lane| lane.run == run) {
            lane.disposition = RunDisposition::Stopped;
            for (slot, id) in owned.iter_mut().zip(self.scenes.keys()) {
                if *id == lane.scene {
                    *slot = Some(*id);
                }
            }
        }
        for (frame, _, disposition) in &mut self.held {
            if let DataToShell::SceneStepCommit {
                run: owner, scene, ..
            } = &**frame
                && *owner == run
            {
                *disposition = RunDisposition::Stopped;
                for (slot, id) in owned.iter_mut().zip(self.scenes.keys()) {
                    if id == scene {
                        *slot = Some(*id);
                    }
                }
            }
        }
        self.discard_owned_replacement(run);
        if let Some(state) = &mut self.conduction {
            state.fence(|key| key.run == run || owned.contains(&Some(key.scene)));
        }
        let mut cuts = Vec::new();
        for id in owned.into_iter().flatten() {
            let scene = self.scenes.get_mut(&id).expect("owned installed scene");
            scene.halt();
            scene.skipping = true;
            scene.stopped_ms = scene.stopped_ms.or(scene.sampled_ms);
            if scene.cut == Cut::Open {
                scene.cut = Cut::Outstanding;
                cuts.push(SceneCut {
                    scene: id,
                    cut: scene.cut_now(),
                });
            }
        }
        let mut out = Out::default();
        self.drain(now, &mut out)?;
        out.flush();
        out.effects
            .push(Effect::Control(ControlToBrain::StopScenes {
                run,
                cuts: SceneCuts::new(cuts).expect("at most four owned scenes"),
            }));
        Ok(out.finish())
    }

    /// The second Esc: every scene goes, recovery included, in one `ClearDrawing`.
    /// # Errors
    /// [`Broken`] when a held frame applied after clearing recovery can't be mirrored.
    pub fn clear(&mut self, now: Instant) -> Result<Vec<Effect>, Broken> {
        let successor = self.frames.borrow().successor();
        self.discard_replacement();
        self.stopped_begins = [None; MAX_VISIBLE_SCENES];
        let mut out = Out::default();
        if self.recovery_incomplete() {
            self.abandon(now, &mut out)?;
        } else if !matches!(self.phase, Phase::Live) {
            out.credit(self.assembler.clear().credit);
            self.clear_after_recovery = true;
        }
        self.cancel_admission(None, &mut out);
        if matches!(self.phase, Phase::Live) {
            self.discard_held(now, &mut out)?;
        }
        let ids: Vec<_> = self.scenes.keys().copied().collect();
        for id in ids {
            self.release(id);
            self.forget(id, &mut out);
        }
        if let Some(id) = successor
            && !self.cleared.contains(&id)
        {
            self.forget(id, &mut out);
        }
        Ok(out.finish())
    }

    /// This connection's v1 membership arrived, empty or not.
    pub fn membership_ready(&self) -> bool {
        self.assembler.described()
    }

    /// Whether a v1 scene is mirrored or recovering, so Esc sends v1 cuts, not the placeholder Stop.
    pub fn holds_scenes(&self) -> bool {
        self.frames.borrow().retirement.is_some()
            || !self.scenes.is_empty()
            || matches!(self.phase, Phase::Recovering(_))
            || self.assembler.described() && self.assembler.pending()
    }

    /// Prepared geometry charged against [`MAX_RESIDENT_BYTES`]: what each scene retains with
    /// its view and frame, its prepared steps, and every consumed step still held.
    pub(crate) fn residency(&self) -> usize {
        self.scenes.values().map(Scene::residency).fold(
            self.returned
                .load(Ordering::Relaxed)
                .saturating_add(self.frames.borrow().retirement_bytes())
                .saturating_add(self.stage.fixed_bytes()),
            usize::saturating_add,
        )
    }

    fn paint_work(&self) -> PaintWork {
        self.scenes
            .values()
            .fold(self.frames.borrow().retirement_work(), |sum, scene| {
                sum + scene.paint
            })
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

impl Presenter {
    /// The brain removed scenes: they go now, in any phase, and nothing of them installs later.
    fn removed(&mut self, scenes: &[SceneId], now: Instant, out: &mut Out) -> Result<(), Broken> {
        if self
            .frames
            .borrow()
            .successor()
            .is_some_and(|id| scenes.contains(&id))
        {
            self.discard_replacement();
        }
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
        frame: Deferred<DataToShell>,
        bytes: u32,
        disposition: RunDisposition,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        let commit = match *frame {
            DataToShell::SceneStepCommit {
                run, scene, index, ..
            } => Some((run, scene, index)),
            _ => None,
        };
        if let Some(commit) = commit {
            self.frames.borrow_mut().bind_successor(commit.1, commit.0);
            if disposition == RunDisposition::Stopped {
                self.discard_owned_replacement(commit.0);
            }
            if self.fenced.is_some_and(|f| commit.0 <= f)
                && self.frames.borrow().successor() == Some(commit.1)
            {
                self.discard_replacement();
            }
            return self.admit(commit, frame, bytes, disposition, now, out);
        }
        out.credit(bytes);
        match &*frame {
            DataToShell::SceneBegin { scene, header } => {
                if self.cleared.contains(scene) {
                    return Ok(());
                }
                if self.scenes.contains_key(scene) {
                    return Err(Broken::Order);
                }
                if header.layout_version != LAYOUT_VERSION {
                    return Err(Broken::Header);
                }
                if self.scenes.len() >= MAX_VISIBLE_SCENES {
                    return Err(Broken::Scenes);
                }
                let state = SceneState::begin(header.begin.clone()).map_err(|_| Broken::Header)?;
                let rest = LayoutState::new(&state);
                let retained = rest.resident_bytes();
                let view = placement::resting_view(
                    &rest,
                    state.activated(),
                    self.drawing_stage,
                    state.header(),
                    self.display,
                );
                let activation = Activation::new(&state, now);
                let frame = Frame::new();
                let cache = self.stage.stage(&view);
                let picture = retained
                    + view.resident_bytes()
                    + Frames::reservation(&frame)
                    + cache.bytes()
                    + Activation::BYTES;
                self.make_room(
                    self.residency().saturating_add(picture),
                    self.paint_work() + rest.paint_work(),
                );
                if self.residency().saturating_add(picture) > MAX_RESIDENT_BYTES
                    || !(self.paint_work() + rest.paint_work()).within_limits()
                {
                    let bytes = state.attributed_bytes().saturating_add(picture);
                    drop(Deferred::new(
                        (state, rest, view, frame, cache, activation),
                        bytes,
                    ));
                    self.forget_failed(*scene, out);
                    return Ok(());
                }
                let bytes = state.attributed_bytes();
                let state = SceneBinding::bind(Deferred::new(state, bytes), &mut self.global)
                    .map_err(|_| Broken::Scenes)?;
                self.install(
                    *scene, state, rest, retained, view, frame, cache, activation,
                );
                if on_screen(header) {
                    let mirrored = self.scenes.get_mut(scene);
                    mirrored.expect("the scene was just installed").screen = true;
                } else {
                    // A replaced screen scene's successor that is no screen scene ends the watch.
                    self.watch.gone(*scene);
                    self.hush();
                }
                self.make_room(self.residency(), self.paint_work());
                Ok(())
            }
            DataToShell::SceneCutApplied {
                scene,
                revision,
                cut,
            } => self.cut_applied(*scene, *revision, cut, out),
            DataToShell::ScenesRemoved { scenes } => {
                self.removed(scenes, now, out)?;
                self.narration_retirement_consumed(RetirementLane::Scenes);
                Ok(())
            }
            DataToShell::ScenesReplaced { scenes, successor } => {
                self.replaced(scenes, *successor, now, out)?;
                self.narration_retirement_consumed(RetirementLane::Scenes);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Installs the lane's answer, or expires the lane if the answer is late. Any other answer is
    /// stale and only drops. A timely rejection means the mirror disagrees with the brain.
    fn admitted(
        &mut self,
        job: u64,
        read: SceneRead,
        result: Option<Result<DetachedCandidate, StepError>>,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        let Some(lane) = self.lane.take_if(|lane| lane.id == job) else {
            return Ok(());
        };
        if now >= lane.deadline() {
            self.expire(lane, out);
            return Ok(());
        }
        let Lane {
            run,
            scene: id,
            index,
            eligible_at,
            bytes,
            disposition,
            ..
        } = lane;
        let fenced = self.fenced.is_some_and(|fenced| run <= fenced);
        let scene = self.scenes.get_mut(&id).ok_or(Broken::Order)?;
        let candidate = result.ok_or(Broken::Step)?.map_err(|_| Broken::Step)?;
        let preparation = scene
            .state
            .apply_detached(read, candidate, &mut self.global)
            .map_err(|_| Broken::Step)?;
        let retained = preparation.attributed_bytes();
        let preparation = Deferred::new(preparation, retained);
        out.credit(bytes);
        let stopped = disposition == RunDisposition::Stopped;
        if let Some(pending) = scene.pending_consumption.filter(|_| !stopped) {
            out.effects.push(Effect::ConsumptionReady {
                scene: id,
                index: pending,
            });
        }
        let applied = scene.cut == Cut::Applied;
        if applied {
            scene.cut = Cut::Open;
        }
        if scene.cut == Cut::Outstanding || fenced || stopped {
            scene.skipping = true;
            // A stopped run's late step on a scene its Stop skipped as already cut: this commit
            // allows one new cut, and only the shell can send it.
            if (fenced && applied || stopped) && scene.cut != Cut::Outstanding {
                scene.cut = Cut::Outstanding;
                scene.stopped_ms = scene.sampled_ms;
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
            eligible_at,
            preparation,
        });
        self.make_room(self.residency(), self.paint_work());
        Ok(())
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
            if cut.activated == 0 {
                self.narration_retirement_consumed(RetirementLane::Cut);
            }
            return Ok(());
        };
        if scene.state.view().revision() != revision {
            return Err(Broken::Cut);
        }
        scene.halt();
        let logged = scene.state.view().recovery_log().len();
        let outcome = scene
            .state
            .stop(cut, &mut self.global)
            .map_err(|_| Broken::Cut)?;
        scene.activation.retain(scene.state.view());
        scene.cut = Cut::Applied;
        scene.skipping = false;
        if let Some(state) = &mut self.conduction {
            state.fence(|key| key.scene == id);
        }
        let stopped = scene.stopped_ms.take();
        // An unlogged cut of a view at rest when its Stop started leaves the view and frame
        // current; a moving, delayed or spinning view takes the cut path to its end values.
        if outcome == StopOutcome::Kept
            && scene.state.view().recovery_log().len() == logged
            && scene.view.activated() == scene.state.view().activated()
            && stopped.is_some_and(|at| scene.view.at_rest(at))
        {
            return Ok(());
        }
        let rest = match outcome {
            StopOutcome::Kept => scene.rest.cut(scene.state.view()).ok(),
            StopOutcome::Cleared => None,
        };
        if let Some(rest) = rest {
            let mut view = placement::resting_view(
                &rest,
                scene.state.view().activated(),
                self.drawing_stage,
                scene.state.view().header(),
                self.display,
            );
            if let Some(at) = stopped {
                view.carry_spin(&scene.view, at);
            }
            let cache = self.stage.stage(&view);
            let cache_bytes = cache.bytes();
            let old = (
                std::mem::replace(&mut scene.rest, rest),
                std::mem::replace(&mut scene.view, view),
                self.frames.borrow_mut().replaced(id, cache),
            );
            drop(Deferred::new(
                old,
                scene.retained + scene.view_bytes + scene.cache_bytes,
            ));
            scene.retained = scene.rest.resident_bytes();
            scene.view_bytes = scene.view.resident_bytes();
            scene.cache_bytes = cache_bytes;
            scene.paint = scene.rest.paint_work();
            if self.residency() > MAX_RESIDENT_BYTES || !self.paint_work().within_limits() {
                self.release(id);
                self.forget(id, out);
            }
        } else {
            self.release(id);
            if outcome == StopOutcome::Kept {
                self.forget(id, out);
            } else {
                self.remember(id);
            }
        }
        if cut.activated == 0 {
            self.narration_retirement_consumed(RetirementLane::Cut);
        }
        Ok(())
    }

    /// Starts waiting work, the admission lane and its held frames ahead of scene steps.
    fn pump_all(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        self.dispatch_recovery(now, out)?;
        self.dispatch_admission(now, out);
        self.drain(now, out)?;
        let ids: Vec<_> = self.scenes.keys().copied().collect();
        for id in ids {
            self.pump(id, now, out)?;
        }
        Ok(())
    }

    /// Stamps a scene's next step eligible once its predecessor is ready and the window has room,
    /// then starts it on the tip its previous step left when a worker slot is free.
    #[expect(clippy::result_large_err, reason = "preserve rejected ownership")]
    fn pump(&mut self, id: SceneId, now: Instant, out: &mut Out) -> Result<(), Broken> {
        if let Some(scene) = self.scenes.get_mut(&id)
            && scene.open()
        {
            match (scene.queue.front_mut(), &mut self.lane) {
                (Some(ticket), _) => _ = ticket.eligible_at.get_or_insert(now),
                (None, Some(lane)) if lane.scene == id => _ = lane.eligible_at.get_or_insert(now),
                _ => {}
            }
        }
        if self.workers() >= MAX_WORKERS || !self.dropper_running() {
            return Ok(());
        }
        let job = self.next_job;
        let Some(scene) = self.scenes.get_mut(&id) else {
            return Ok(());
        };
        if scene.running.is_some() || scene.cut == Cut::Outstanding {
            return Ok(());
        }
        let Some(eligible_at) = scene.queue.front().and_then(|ticket| ticket.eligible_at) else {
            return Ok(());
        };
        let ticket = scene
            .queue
            .pop_front()
            .expect("the front ticket was just read");
        let (run, index) = (ticket.run, ticket.index);
        if now >= eligible_at + DEADLINE {
            return self.fail(id, run, index, SceneFailure::Timeout, now, out);
        }
        let bytes = ticket.preparation.attribution()
            + scene
                .ready
                .back()
                .map_or(scene.retained, |ready| ready.full_after);
        let input = ticket
            .preparation
            .map(|preparation| {
                PrepareInput::new(
                    preparation,
                    scene.tip().clone(),
                    self.surface,
                    Arc::clone(&self.theme),
                )
            })
            .transpose();
        match input {
            Ok(mut input) => {
                input.attribute(bytes);
                let cancel = Arc::new(AtomicBool::new(false));
                scene.running = Some(Running {
                    id: job,
                    run,
                    index,
                    eligible_at,
                    cancel: Arc::clone(&cancel),
                });
                self.next_job += 1;
                let slot = Arc::clone(&self.slots);
                out.effects.push(Effect::Prepare(PrepareJob::new(
                    job, id, input, cancel, slot,
                )));
                Ok(())
            }
            Err(error) if matches!(error.0, PrepareError::Stale) => Err(Broken::Order),
            Err(error) => self.fail(id, run, index, failure(&error.0), now, out),
        }
    }

    /// Publishes a current answer, or fails its step: late, invalid, stale or over residency.
    fn prepared(
        &mut self,
        id: SceneId,
        running: &Running,
        result: Deferred<Result<PreparedStep, PrepareError>>,
        full_after: usize,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        let elapsed = now.saturating_duration_since(running.eligible_at);
        if let Ok(prepared) = &*result
            && let Some(scene) = self.scenes.get(&id)
        {
            let other = self
                .scenes
                .iter()
                .filter(|(scene, _)| **scene != id)
                .fold(self.frames.borrow().retirement_work(), |sum, (_, scene)| {
                    sum + scene.paint
                });
            self.make_room(
                self.residency().saturating_add(prepared.resident_bytes),
                other + scene.paint.max(prepared.after.paint_work()),
            );
        }
        let total = self.residency();
        let other_work = self
            .scenes
            .iter()
            .filter(|(scene, _)| **scene != id)
            .fold(self.frames.borrow().retirement_work(), |sum, (_, scene)| {
                sum + scene.paint
            });
        let Some(scene) = self.scenes.get_mut(&id) else {
            return Ok(());
        };
        let accepted = if elapsed >= DEADLINE {
            Err(SceneFailure::Timeout)
        } else {
            result.transpose().map_err(|error| failure(&error))
        }
        .and_then(|prepared| {
            let paint = scene.paint.max(prepared.after.paint_work());
            if prepared.index != running.index
                || prepared.validate(scene.state.view(), scene.tip()).is_err()
            {
                Err(SceneFailure::Validation)
            } else if total.saturating_add(prepared.resident_bytes) > MAX_RESIDENT_BYTES {
                Err(SceneFailure::Residency)
            } else if !(other_work + paint).within_limits() {
                Err(SceneFailure::PaintWork)
            } else {
                Ok((prepared, paint))
            }
        });
        match accepted {
            Ok((prepared, paint)) => {
                if elapsed > TARGET {
                    let elapsed_ms = elapsed.as_millis();
                    tracing::warn!(elapsed_ms, "a scene step's preparation missed its target");
                }
                scene.paint = paint;
                scene.ready.push_back(Ready {
                    run: running.run,
                    step: prepared,
                    full_after,
                });
                self.ready.push(ControlToBrain::StepReady {
                    run: running.run,
                    scene: id,
                    index: running.index,
                });
                if let Some(observe) = self.draw_timing.as_mut() {
                    observe(DrawTiming::Ready(DrawKey {
                        run: running.run,
                        scene: id,
                        index: running.index,
                    }));
                }
            }
            Err(reason) => self.fail(id, running.run, running.index, reason, now, out)?,
        }
        Ok(())
    }

    /// The steps prepared since the last call, in order, each to tell the brain once; the caller
    /// sends them ahead of the effects of the answer that prepared them.
    pub fn take_ready(&mut self) -> Vec<Effect> {
        self.ready.drain(..).map(Effect::Control).collect()
    }

    /// A step failed: its scene drops its work and, unless already cut since its last commit,
    /// sends the cut at its activated bound and waits for the brain to acknowledge it. A
    /// consumption the scene was waiting for goes with its work, so the frames held behind it
    /// resume.
    fn fail(
        &mut self,
        id: SceneId,
        run: RunId,
        index: u32,
        reason: SceneFailure,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        if self.frames.borrow().successor() == Some(id) {
            self.discard_replacement();
        }
        let Some(scene) = self.scenes.get_mut(&id) else {
            return Ok(());
        };
        let waited = scene.pending_consumption.is_some();
        scene.halt();
        scene.skipping = true;
        if scene.cut == Cut::Open {
            scene.cut = Cut::Outstanding;
            scene.stopped_ms = scene.sampled_ms;
            out.effects
                .push(Effect::Control(ControlToBrain::StepFailed {
                    run,
                    scene: id,
                    index,
                    reason,
                    cut: scene.cut_now(),
                }));
        }
        if waited {
            self.drain(now, out)?;
        }
        Ok(())
    }

    /// Drops a scene and releases its budget, residency and work.
    fn release(&mut self, id: SceneId) {
        if self.frames.borrow().successor() == Some(id) {
            self.discard_replacement();
        }
        if let Some(mut scene) = self.scenes.remove(&id) {
            scene.halt();
            let bytes = scene.held();
            let Scene {
                state,
                rest,
                view,
                activation,
                ..
            } = scene;
            state.release(&mut self.global).expect(BOUND);
            let frame = self.frames.borrow_mut().take(id);
            drop(Deferred::new((rest, view, frame, activation), bytes));
        }
    }

    /// A terminal whole-scene refusal, distinct from an explicit user clear.
    fn forget_failed(&mut self, id: SceneId, out: &mut Out) {
        if !self.cleared.contains(&id) {
            self.remember(id);
            out.effects
                .push(Effect::Control(ControlToBrain::SceneFailed { scene: id }));
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
        if self.watch.gone(id) {
            left();
        }
        self.hush();
        for pending in &mut self.stopped_begins {
            if *pending == Some(id) {
                *pending = None;
            }
        }
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

    /// Whether discarded values still leave the UI thread; logs once when they stopped.
    fn dropper_running(&mut self) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        let running = self.dropper.unwrap_or_else(deferred::running);
        #[cfg(not(any(test, feature = "test-support")))]
        let running = deferred::running();
        if !running && !self.refused {
            self.refused = true;
            tracing::error!("the scene dropper stopped; scene intake and preparation stop");
        }
        running
    }

    fn job_id(&mut self) -> u64 {
        let id = self.next_job;
        self.next_job += 1;
        id
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.disconnected();
    }
}

/// A frame's wire size, as the bytes attributed to it while it waits for the dropper.
fn wire_bytes(bytes: u32) -> usize {
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

fn failure(error: &PrepareError) -> SceneFailure {
    match error {
        PrepareError::Cancelled => SceneFailure::Cancelled,
        PrepareError::Stale => SceneFailure::Validation,
        PrepareError::TextMeasure => SceneFailure::TextMeasure,
        PrepareError::OverResidency => SceneFailure::Residency,
        PrepareError::OverPaintWork => SceneFailure::PaintWork,
        PrepareError::Layout(_) => SceneFailure::Layout,
    }
}

#[cfg(any(test, feature = "test-support"))]
#[expect(
    clippy::missing_assert_message,
    reason = "the fixture's asserts are test assertions, built outside cfg(test) for the shell's tests"
)]
pub mod fixture;
#[cfg(test)]
pub(crate) mod tests;
