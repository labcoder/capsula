//! The one admission lane. Each `StepCommit`, in arrival order, is admitted on a worker against a
//! read-only snapshot of its scene and installed through the scene's binding on the UI thread.
//! Every later scene frame waits in `held` behind it, so nothing mutates a scene whose snapshot
//! is out. Past its deadline only its scene goes: released, tombstoned and cleared.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use kumochi_protocol::message::{DataToShell, RunId, SceneId};
use kumochi_scene::deferred::Deferred;

use super::{
    AdmitJob, Broken, DEADLINE, Effect, MAX_WORKERS, Out, Phase, Presenter, RetirementLane,
    RunDisposition, wire_bytes,
};

pub(super) struct Lane {
    pub(super) id: u64,
    pub(super) scene: SceneId,
    pub(super) run: RunId,
    pub(super) index: u32,
    /// When it reached the lane; its deadline covers the slot wait and the worker.
    entered: Instant,
    /// When its preparation predecessor was ready and the window had room, once that happened.
    pub(super) eligible_at: Option<Instant>,
    /// Its frame's withheld credit.
    pub(super) bytes: u32,
    pub(super) disposition: RunDisposition,
    /// The frame until a worker slot frees for it.
    frame: Option<Deferred<DataToShell>>,
    cancel: Arc<AtomicBool>,
}

impl Lane {
    pub(super) fn deadline(&self) -> Instant {
        self.entered + DEADLINE
    }

    pub(super) fn cancel(self) -> u32 {
        self.cancel.store(true, Ordering::Relaxed);
        self.bytes
    }
}

impl Presenter {
    pub(super) fn waits_for_consumption(&self, frame: &DataToShell) -> bool {
        matches!(frame, DataToShell::SceneStepCommit { scene, .. }
            if self.scenes.get(scene).is_some_and(|scene| scene.pending_consumption.is_some()))
    }

    /// A commit at the head of the stream enters the lane once its scene and index check out.
    pub(super) fn admit(
        &mut self,
        (run, id, index): (RunId, SceneId, u32),
        frame: Deferred<DataToShell>,
        bytes: u32,
        disposition: RunDisposition,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        if self.cleared.contains(&id) {
            out.credit(bytes);
            return Ok(());
        }
        let scene = self.scenes.get(&id).ok_or(Broken::Order)?;
        if index != scene.state.view().revision() {
            return Err(Broken::Index);
        }
        if disposition == RunDisposition::Stopped {
            let scene = self
                .scenes
                .get_mut(&id)
                .expect("structurally checked scene");
            scene.halt();
            scene.skipping = true;
            scene.stopped_ms = scene.stopped_ms.or(scene.sampled_ms);
        }
        let scene = self.scenes.get(&id).expect("structurally checked scene");
        let eligible_at = (scene.open() && scene.queue.is_empty()).then_some(now);
        self.lane = Some(Lane {
            id: self.job_id(),
            scene: id,
            run,
            index,
            entered: now,
            eligible_at,
            bytes,
            disposition,
            frame: Some(frame),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        self.dispatch_admission(now, out);
        Ok(())
    }

    /// Expires the lane at its deadline, or sends its job once a worker slot is free.
    pub(super) fn dispatch_admission(&mut self, now: Instant, out: &mut Out) {
        if let Some(lane) = self.lane.take_if(|lane| now >= lane.deadline()) {
            self.expire(lane, out);
            return;
        }
        if self.lane.as_ref().is_none_or(|lane| lane.frame.is_none())
            || self.workers() >= MAX_WORKERS
            || !self.dropper_running()
        {
            return;
        }
        let lane = self.lane.as_mut().expect("a waiting lane was just read");
        let frame = lane.frame.take().expect("a waiting lane holds its frame");
        let read = self
            .scenes
            .get(&lane.scene)
            .expect("the lane's scene stays mirrored while the lane is out")
            .state
            .read();
        out.effects.push(Effect::Admit(AdmitJob::new(
            lane.id,
            read,
            frame,
            Arc::clone(&lane.cancel),
            Arc::clone(&self.slots),
        )));
    }

    /// Cancels the admission that is out, or only `scene`'s own, and returns its withheld credit
    /// once. Its late answer then finds no lane and drops. A screen scene's retirement and the
    /// clear of every scene come through here first, so no answer arrives for a scene they took.
    pub(super) fn cancel_admission(&mut self, scene: Option<SceneId>, out: &mut Out) {
        let lane = self
            .lane
            .take_if(|lane| scene.is_none_or(|scene| lane.scene == scene));
        if let Some(lane) = lane {
            out.credit(lane.cancel());
        }
    }

    /// Policy A: a late admission clears its whole scene, releasing only what was mirrored.
    pub(super) fn expire(&mut self, lane: Lane, out: &mut Out) {
        tracing::warn!("a scene step's admission missed its deadline; clearing its scene");
        let scene = lane.scene;
        out.credit(lane.cancel());
        self.release(scene);
        self.forget_failed(scene, out);
    }

    /// Applies held frames in arrival order until one enters the lane. A commit for a scene with
    /// a consumption waiting stays held until that consumption is served, its scene halts, or a
    /// wake that can no longer serve it drops it.
    pub(super) fn drain(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        while self.lane.is_none() && matches!(self.phase, Phase::Live) {
            let Some((frame, _, _)) = self.held.front() else {
                return Ok(());
            };
            if self.waits_for_consumption(frame) {
                return Ok(());
            }
            let (frame, bytes, disposition) = self
                .held
                .pop_front()
                .expect("the front frame was just read");
            self.held_bytes -= bytes;
            self.live(frame, bytes, disposition, now, out)?;
        }
        Ok(())
    }

    /// Drops every held frame with its credit. Scenes they began are cleared, and scenes they
    /// removed are only forgotten, as if each had applied before a clear.
    pub(super) fn discard_held(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        let held = Deferred::new(std::mem::take(&mut self.held), wire_bytes(self.held_bytes));
        out.credit(std::mem::take(&mut self.held_bytes));
        let mut begun = Vec::new();
        for (frame, _, _) in &*held {
            match &**frame {
                DataToShell::SceneBegin { scene, .. } => begun.push(*scene),
                DataToShell::ScenesRemoved { scenes }
                | DataToShell::ScenesReplaced { scenes, .. } => {
                    self.removed(scenes, now, out)?;
                    self.narration_retirement_consumed(RetirementLane::Scenes);
                }
                DataToShell::SceneCutApplied { scene, cut, .. } if cut.activated == 0 => {
                    self.removed(std::slice::from_ref(scene), now, out)?;
                    self.narration_retirement_consumed(RetirementLane::Cut);
                }
                _ => {}
            }
        }
        for scene in begun {
            self.forget(scene, out);
        }
        Ok(())
    }
}
