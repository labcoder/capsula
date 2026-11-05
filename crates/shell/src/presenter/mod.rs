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
//! - Only the oldest prepared step is consumed, and its layout becomes the rest. Before B6 nothing
//!   consumes in production, so a cut or failure names the activated bound: the recovered resting
//!   layout, or zero.
//! - A scene with a cut outstanding prepares nothing until the brain acknowledges the cut, and a
//!   scene gets at most one cut between commits.
//! - Scene frames, jobs, answers and scenes are held in [`Deferred`] from the moment the
//!   presenter owns them, so whichever path discards them, they drop on the dropper thread. Once the
//!   dropper has stopped, intake and new work stop with it, and the watchdog fails what waits.

mod admit;
mod measure;
pub(crate) mod overlay_input;
mod paint;
mod prepare;
mod recovery;
mod scene;
mod snapshot;
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
    DetachedCandidate, Frame, GlobalBudget, SceneBinding, SceneRead, SceneState, SceneView,
    StepError, StopCut, StopOutcome,
    deferred::{self, Deferred},
    display::PaintWork,
    layout::{LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry},
    limits::MAX_RESIDENT_BYTES,
    text::Bounded,
    theme::Theme,
};

use admit::Lane;
pub(crate) use measure::Measure;
pub(crate) use paint::Frames;
pub(crate) use prepare::{AdmitJob, Completion, PrepareJob, RecoveryJob};
use recovery::Phase;
use scene::{Cut, Out, Ready, Running, Scene, Ticket};
use snapshot::Assembler;

/// The layout algorithm this shell prepares with; the brain stores it with each scene.
const LAYOUT_VERSION: u32 = 1;
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
    /// Run on a background executor, then pass the answer to [`Presenter::completed`].
    Admit(AdmitJob),
    /// A consumption refused as busy can be retried now, before this scene admits again.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "B6's activation owner is the production caller")
    )]
    ConsumptionReady { scene: SceneId, index: u32 },
}

/// Every v1 scene the shell mirrors, and the work preparing them.
pub(crate) struct Presenter {
    pub(crate) input: overlay_input::OverlayOwner,
    surface: SurfaceGeometry,
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
    held: VecDeque<(Deferred<DataToShell>, u32)>,
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
    /// The dropper stopped and intake was refused; logged once.
    refused: bool,
    /// Consumed steps their owners still hold, charged until each is dropped, across connections.
    returned: Arc<AtomicUsize>,
    /// This presenter's dropper liveness in tests; `None` asks the process-wide dropper.
    #[cfg(test)]
    dropper: Option<bool>,
}

impl Presenter {
    pub(crate) fn new(surface: SurfaceGeometry, theme: Arc<Theme>) -> Self {
        Self {
            input: overlay_input::OverlayOwner::default(),
            surface,
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
            cleared: VecDeque::new(),
            next_job: 0,
            slots: Arc::new(()),
            refused: false,
            returned: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            dropper: None,
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
        if let Some(lane) = self.lane.take() {
            lane.cancel();
        }
        for scene in self.scenes.values_mut() {
            scene.halt();
        }
        let bytes = self.scenes.values().map(Scene::attributed).sum();
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
    /// and `ScenesRemoved` are the presenter's; any other frame does nothing and earns no credit,
    /// and so does every frame once the dropper has stopped. A stale frame is still the
    /// presenter's to drop.
    /// # Errors
    /// [`Broken`] for a frame the mirror can't apply.
    #[expect(clippy::result_large_err, reason = "preserve rejected ownership")]
    pub(crate) fn data(
        &mut self,
        generation: u64,
        frame: Deferred<DataToShell>,
        bytes: u32,
        now: Instant,
    ) -> Result<Vec<Effect>, Broken> {
        let mut out = Out::default();
        let current = self.generation == Some(generation) && self.dropper_running();
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
                let removed = matches!(&*frame, DataToShell::ScenesRemoved { .. });
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
                    self.live(frame, bytes, now, &mut out)?;
                } else if scene {
                    let lane = self.lane.as_ref().map_or(0, |lane| lane.bytes);
                    self.held_bytes = self
                        .held_bytes
                        .checked_add(bytes)
                        .filter(|held| held.saturating_add(lane) <= DATA_WINDOW)
                        .ok_or(Broken::Window)?;
                    self.held.push_back((frame, bytes));
                }
            }
            Ok(_) | Err(_) => {}
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
                full_after,
                slot,
            } => {
                drop(slot);
                let running = self
                    .scenes
                    .get_mut(&scene)
                    .and_then(|state| state.running.take_if(|running| running.id == id));
                if let Some(running) = running {
                    self.prepared(scene, &running, result, full_after, now, &mut out);
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

    /// The earliest deadline the watchdog must check, if any work is timed.
    pub(crate) fn deadline(&self) -> Option<Instant> {
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
        if let Some(lane) = self.lane.take() {
            out.credit(lane.cancel());
        }
        if matches!(self.phase, Phase::Live) {
            self.discard_held(now, &mut out)?;
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

    /// Prepared geometry charged against [`MAX_RESIDENT_BYTES`]: what each scene retains with
    /// its view and frame, its prepared steps, and every consumed step still held.
    pub(crate) fn residency(&self) -> usize {
        self.scenes
            .values()
            .map(Scene::residency)
            .fold(self.returned.load(Ordering::Relaxed), usize::saturating_add)
    }

    fn paint_work(&self) -> PaintWork {
        self.scenes
            .values()
            .fold(PaintWork::default(), |sum, scene| sum + scene.paint)
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
            return self.admit(commit, frame, bytes, now, out);
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
                let view = SceneView::new(&rest, state.activated());
                let frame = Frame::new();
                let picture = retained + view.resident_bytes() + Frames::reservation(&frame);
                if self.residency().saturating_add(picture) > MAX_RESIDENT_BYTES
                    || !(self.paint_work() + rest.paint_work()).within_limits()
                {
                    let bytes = state.attributed_bytes().saturating_add(picture);
                    drop(Deferred::new((state, rest, view, frame), bytes));
                    self.forget(*scene, out);
                    return Ok(());
                }
                let bytes = state.attributed_bytes();
                let state = SceneBinding::bind(Deferred::new(state, bytes), &mut self.global)
                    .map_err(|_| Broken::Scenes)?;
                self.install(*scene, state, rest, retained, view, frame);
                Ok(())
            }
            DataToShell::SceneCutApplied {
                scene,
                revision,
                cut,
            } => self.cut_applied(*scene, *revision, cut, out),
            DataToShell::ScenesRemoved { scenes } => self.removed(scenes, now, out),
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
            ..
        } = lane;
        let fenced = self.fenced.is_some_and(|fenced| run <= fenced);
        let scene = self.scenes.get_mut(&id).ok_or(Broken::Order)?;
        let candidate = result.ok_or(Broken::Step)?.map_err(|_| Broken::Step)?;
        let preparation = scene
            .state
            .apply_detached(read, candidate, &mut self.global)
            .map_err(|_| Broken::Step)?;
        let preparation = Deferred::new(preparation, scene.state.view().attributed_bytes());
        out.credit(bytes);
        if let Some(pending) = scene.pending_consumption {
            out.effects.push(Effect::ConsumptionReady {
                scene: id,
                index: pending,
            });
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
            eligible_at,
            preparation,
        });
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
            return Ok(());
        };
        if scene.state.view().revision() != revision {
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
            StopOutcome::Kept => scene.rest.cut(scene.state.view()).ok(),
            StopOutcome::Cleared => None,
        };
        if let Some(rest) = rest {
            let view = SceneView::new(&rest, scene.state.view().activated());
            let old = (
                std::mem::replace(&mut scene.rest, rest),
                std::mem::replace(&mut scene.view, view),
            );
            self.frames.borrow_mut().replaced(id);
            drop(Deferred::new(old, scene.retained + scene.view_bytes));
            scene.retained = scene.rest.resident_bytes();
            scene.view_bytes = scene.view.resident_bytes();
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
            self.fail(id, run, index, SceneFailure::Timeout, out);
            return Ok(());
        }
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
                input.attribute(scene.attributed());
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
            Err(error) => {
                self.fail(id, run, index, failure(&error.0), out);
                Ok(())
            }
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
    ) {
        let elapsed = now.saturating_duration_since(running.eligible_at);
        let total = self.residency();
        let other_work = self
            .scenes
            .iter()
            .filter(|(scene, _)| **scene != id)
            .fold(PaintWork::default(), |sum, (_, scene)| sum + scene.paint);
        let Some(scene) = self.scenes.get_mut(&id) else {
            return;
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
            } else if total.saturating_add(prepared.resident_bytes) > MAX_RESIDENT_BYTES
                || !(other_work + paint).within_limits()
            {
                Err(SceneFailure::Residency)
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
                    step: prepared,
                    full_after,
                });
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
            let bytes = scene.attributed();
            let Scene {
                state, rest, view, ..
            } = scene;
            state.release(&mut self.global).expect(BOUND);
            let frame = self.frames.borrow_mut().take(id);
            drop(Deferred::new((rest, view, frame), bytes));
        }
    }

    /// Adopts a bound scene and reserves its frame.
    fn install(
        &mut self,
        id: SceneId,
        state: SceneBinding,
        rest: LayoutState,
        retained: usize,
        view: SceneView,
        frame: Frame,
    ) {
        let frame = self.frames.borrow_mut().adopt(id, frame);
        self.scenes
            .insert(id, Scene::new(state, rest, retained, view, frame));
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

    /// Whether discarded values still leave the UI thread; logs once when they stopped.
    fn dropper_running(&mut self) -> bool {
        #[cfg(test)]
        let running = self.dropper.unwrap_or_else(deferred::running);
        #[cfg(not(test))]
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
        PrepareError::Layout(_) => SceneFailure::Layout,
    }
}

#[cfg(test)]
pub(crate) mod tests;
