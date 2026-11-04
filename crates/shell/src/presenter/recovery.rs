use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use kumochi_protocol::message::SceneId;
use kumochi_scene::{
    GlobalBudget, SceneEvent, deferred::Deferred, layout::SceneRecovery, limits::MAX_RESIDENT_BYTES,
};

use super::{
    BOUND, Broken, Effect, LAYOUT_VERSION, MAX_WORKERS, Out, Presenter, RECOVERY_DEADLINE,
    RecoveryJob, Scene,
    snapshot::{self, Source},
    wire_bytes,
};

#[derive(Debug)]
pub(super) enum Phase {
    Offline,
    /// Waiting for `ScenesV1` and every chunk it names.
    Assembling,
    /// The batch is being recovered.
    Recovering(Recovery),
    Live,
}

#[derive(Debug)]
pub(super) struct Recovery {
    pub(super) id: u64,
    pub(super) started: Instant,
    pub(super) cancel: Arc<AtomicBool>,
    /// Each scene with its descriptor's revision and retained count.
    pub(super) scenes: Vec<(SceneId, u32, u32)>,
    /// The batch until a worker slot frees for it; none once its job is sent.
    pub(super) waiting: Option<Deferred<Vec<Source>>>,
}

impl Presenter {
    pub(super) fn assembled(
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

    pub(super) fn recover(
        &mut self,
        batch: Deferred<Vec<Source>>,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        let other: Vec<_> = batch
            .iter()
            .filter(|source| source.header.layout_version != LAYOUT_VERSION)
            .map(|source| source.scene)
            .collect();
        for scene in other {
            self.forget(scene, out);
        }
        let sources = batch.map(|batch| {
            let (sources, other): (Vec<_>, Vec<_>) = batch
                .into_iter()
                .partition(|source| source.header.layout_version == LAYOUT_VERSION);
            let bytes = other
                .iter()
                .map(|source| source.log.len() * size_of::<SceneEvent>())
                .sum();
            drop(Deferred::new(other, bytes));
            sources
        });
        if sources.is_empty() {
            self.global = GlobalBudget::default();
            return self.go_live(now, out);
        }
        let id = self.job_id();
        let scenes = sources
            .iter()
            .map(|source| (source.scene, source.revision, source.committed))
            .collect();
        self.phase = Phase::Recovering(Recovery {
            id,
            started: now,
            cancel: Arc::new(AtomicBool::new(false)),
            scenes,
            waiting: Some(sources),
        });
        self.dispatch_recovery(now, out)
    }

    /// Sends a batch that waited for a worker slot, or abandons it once its deadline passed.
    pub(super) fn dispatch_recovery(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        let Phase::Recovering(recovery) = &self.phase else {
            return Ok(());
        };
        if recovery.waiting.is_none() {
            return Ok(());
        }
        if now >= recovery.started + RECOVERY_DEADLINE {
            return self.abandon(now, out);
        }
        if self.workers() >= MAX_WORKERS || !self.dropper_running() {
            return Ok(());
        }
        let Phase::Recovering(recovery) = &mut self.phase else {
            return Ok(());
        };
        let Some(sources) = recovery.waiting.take() else {
            return Ok(());
        };
        out.effects.push(Effect::Recover(RecoveryJob::new(
            recovery.id,
            sources,
            self.surface,
            Arc::clone(&self.theme),
            Arc::clone(&recovery.cancel),
            Arc::clone(&self.slots),
        )));
        Ok(())
    }

    pub(super) fn recovered(
        &mut self,
        recovery: Recovery,
        scenes: Deferred<Vec<(SceneId, SceneRecovery, usize)>>,
        global: GlobalBudget,
        now: Instant,
        out: &mut Out,
    ) -> Result<(), Broken> {
        self.global = global;
        let mut adopted = Vec::new();
        let rejected = scenes.map(|scenes| {
            let mut results: BTreeMap<_, _> = scenes
                .into_iter()
                .map(|(scene, result, retained)| (scene, (result, retained)))
                .collect();
            let mut rejected = Vec::new();
            for &(scene, revision, committed) in &recovery.scenes {
                // shortcut: a recovery failure clears the whole scene instead of cutting to its prepared prefix; add a scene-keyed RecoveryFailed cut if B9's recovery measurements show failures
                match results.remove(&scene) {
                    Some((Ok((state, rest)), retained))
                        if !self.cleared.contains(&scene)
                            && state.revision() == revision
                            && usize::try_from(committed)
                                .is_ok_and(|kept| kept == state.committed()) =>
                    {
                        adopted.push((scene, state, rest, retained));
                    }
                    Some((Ok((mut state, rest)), _)) => {
                        state.clear(&mut self.global).expect(BOUND);
                        rejected.push((state, rest));
                    }
                    Some((Err(_), _)) | None => {}
                }
            }
            (results, rejected)
        });
        drop(rejected);
        for (scene, state, rest, retained) in adopted {
            self.scenes.insert(scene, Scene::new(state, rest, retained));
        }
        for (scene, ..) in recovery.scenes {
            if !self.scenes.contains_key(&scene) {
                self.forget(scene, out);
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
    pub(super) fn abandon(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
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
    pub(super) fn go_live(&mut self, now: Instant, out: &mut Out) -> Result<(), Broken> {
        self.phase = Phase::Live;
        let mut held = Deferred::new(std::mem::take(&mut self.held), wire_bytes(self.held_bytes));
        self.held_bytes = 0;
        while let Some((frame, bytes)) = held.pop_front() {
            self.live(&frame, bytes, now, out)?;
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

    pub(super) fn recovery_incomplete(&self) -> bool {
        match self.phase {
            Phase::Recovering(_) => true,
            Phase::Assembling => self.assembler.described(),
            Phase::Offline | Phase::Live => false,
        }
    }
}
