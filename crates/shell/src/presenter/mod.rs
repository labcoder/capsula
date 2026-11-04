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

mod measure;
mod prepare;
mod recovery;
mod scene;
mod snapshot;
mod window;

use std::{
    collections::{BTreeMap, VecDeque},
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
    Admission, GlobalBudget, SceneState, Step, StopCut, StopOutcome,
    deferred::{self, Deferred},
    layout::{LayoutState, PrepareError, PrepareInput, PreparedStep, SurfaceGeometry},
    limits::MAX_RESIDENT_BYTES,
    text::Bounded,
    theme::Theme,
};

pub(crate) use measure::Measure;
pub(crate) use prepare::{Completion, PrepareJob, RecoveryJob};
use recovery::Phase;
use scene::{Cut, Out, Ready, Running, Scene, Ticket};
use snapshot::Assembler;

/// The layout algorithm this shell prepares with; the brain stores it with each scene.
const LAYOUT_VERSION: u32 = 1;
/// A step's preparation should take no longer; missing it is only logged.
const TARGET: Duration = Duration::from_millis(50);
/// A step's preparation fails this long after it became eligible, and a later answer is dropped.
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
        for scene in self.scenes.values_mut() {
            scene.halt();
        }
        let bytes = self.scenes.values().map(Scene::attributed).sum();
        drop(Deferred::new(std::mem::take(&mut self.scenes), bytes));
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
            Ok(_) => {}
            Err(frame) => {
                if let DataToShell::ScenesRemoved { scenes } = &*frame
                    && current
                {
                    out.credit(bytes);
                    self.removed(scenes, now, &mut out)?;
                }
                let live = current
                    && matches!(
                        &*frame,
                        DataToShell::SceneBegin { .. }
                            | DataToShell::SceneStepCommit { .. }
                            | DataToShell::SceneCutApplied { .. }
                            | DataToShell::SceneEnd { .. }
                    );
                if live && matches!(self.phase, Phase::Live) {
                    self.live(&frame, bytes, now, &mut out)?;
                } else if live {
                    self.held_bytes = self
                        .held_bytes
                        .checked_add(bytes)
                        .filter(|held| *held <= DATA_WINDOW)
                        .ok_or(Broken::Window)?;
                    self.held.push_back((frame, bytes));
                }
            }
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

    /// Prepared geometry charged against [`MAX_RESIDENT_BYTES`]: what each scene retains, its
    /// prepared steps, and every consumed step still held.
    pub(crate) fn residency(&self) -> usize {
        self.scenes
            .values()
            .map(Scene::residency)
            .fold(self.returned.load(Ordering::Relaxed), usize::saturating_add)
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
        frame: &DataToShell,
        bytes: u32,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        out.credit(bytes);
        match frame {
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
                if self.residency().saturating_add(retained) > MAX_RESIDENT_BYTES {
                    let bytes = state.attributed_bytes().saturating_add(retained);
                    drop(Deferred::new((state, rest), bytes));
                    self.forget(*scene, out);
                    return Ok(());
                }
                self.scenes
                    .insert(*scene, Scene::new(state, rest, retained));
                Ok(())
            }
            DataToShell::SceneStepCommit {
                run,
                scene,
                index,
                step,
            } => self.commit(*run, *scene, *index, step, now, out),
            DataToShell::SceneCutApplied {
                scene,
                revision,
                cut,
            } => self.cut_applied(*scene, *revision, cut, out),
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
        let committed = Deferred::new(committed, scene.state.attributed_bytes());
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
        let preparation = committed.map(|committed| {
            let bytes = committed.handles.capacity()
                * size_of::<(kumochi_scene::WireId, kumochi_scene::Handle)>();
            drop(Deferred::new(committed.handles, bytes));
            committed.preparation
        });
        scene.queue.push_back(Ticket {
            run,
            index,
            eligible_at: None,
            preparation,
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
            let old = std::mem::replace(&mut scene.rest, rest);
            drop(Deferred::new(old, scene.retained));
            scene.retained = scene.rest.resident_bytes();
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
        self.dispatch_recovery(now, out)?;
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
            && let Some(ticket) = scene.queue.front_mut()
        {
            ticket.eligible_at.get_or_insert(now);
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
        let Some(scene) = self.scenes.get_mut(&id) else {
            return;
        };
        let accepted = if elapsed >= DEADLINE {
            Err(SceneFailure::Timeout)
        } else {
            result.transpose().map_err(|error| failure(&error))
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
            scene.state.clear(&mut self.global).expect(BOUND);
            let bytes = scene.attributed();
            drop(Deferred::new(scene, bytes));
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
