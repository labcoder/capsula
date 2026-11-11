//! Visible scenes: commits answered by the store, cuts, retirements, clears, deletions and
//! fifth-scene eviction.

use kumochi_protocol::message::{
    DataToShell, MAX_VISIBLE_SCENES, RunId, SceneHeader, SceneId, ThreadId,
};
use kumochi_scene::text::Bounded;
use kumochi_scene::{Step, StopCut};

use super::io::Write;
use super::{CutKind, Drawing, Held, Job, LAYOUT_VERSION, Output, Pend, Pending, StepWait, Wait};
use crate::coordinator::{Effect, StoreOp};

impl Drawing {
    /// `run` committed `scene`'s step `index`. A scene the brain doesn't hold is stale; any
    /// other mismatch would retire or cut a step the shell was never sent.
    pub(super) fn committed_by(
        &self,
        run: RunId,
        scene: SceneId,
        index: u32,
        out: &mut Output,
    ) -> bool {
        let Some(at) = self.live(scene) else {
            return false;
        };
        let committer = usize::try_from(index)
            .ok()
            .and_then(|index| self.scenes[at].committers.get(index));
        if committer == Some(&Some(run)) {
            return true;
        }
        out.effects
            .push(Effect::Broken("a step its run never committed"));
        false
    }

    /// A step's write committed, so its admission commits too and the shell gets it, after the
    /// header for a new scene. A scene that left meanwhile drops it.
    pub(super) fn stored(&mut self, write: StepWait, id: SceneId, out: &mut Output) {
        let StepWait {
            run,
            call,
            thread,
            scene,
            admitted,
            step,
            interrupted,
            ..
        } = write;
        let begun = matches!(scene, Pend::New(_));
        let at = match scene {
            Pend::Held(scene) => {
                let Some(at) = self.live(scene) else {
                    // Cleared meanwhile, but the step is durable, so it spends the run's budget.
                    self.drew(run, &call, scene, &step);
                    if let Some(error) = interrupted {
                        self.finish(run, &call, Some(error), out);
                    }
                    return;
                };
                at
            }
            Pend::New(state) => {
                let header = SceneHeader {
                    begin: state.header().clone(),
                    layout_version: LAYOUT_VERSION,
                };
                out.effects
                    .push(Effect::Data(DataToShell::SceneBegin { scene: id, header }));
                self.scenes.push(Held {
                    scene: id,
                    thread,
                    layout_version: LAYOUT_VERSION,
                    state: *state,
                    committers: Vec::new(),
                    gone: false,
                });
                self.scenes.len() - 1
            }
        };
        let held = &mut self.scenes[at];
        let committed = held
            .state
            .commit(admitted, &mut self.global)
            .expect("the pending write sealed its scene and the global budget");
        held.committers.push(Some(run));
        let scene = held.scene;
        out.effects.push(Effect::Data(DataToShell::SceneStepCommit {
            run,
            scene,
            index: committed.index,
            step: Step::clone(&step),
        }));
        self.drew(run, &call, scene, &step);
        if begun {
            self.began(thread, scene, out);
        }
        if let Some(error) = interrupted {
            self.finish(run, &call, Some(error), out);
        }
    }

    /// Applies a scene's cut, whichever runs committed what it drops. Every effective cut is
    /// durable before the shell hears it applied; one that changes nothing is acknowledged at
    /// once. A cut the scene rejects can only come from a broken shell.
    pub(super) fn cut(&mut self, scene: SceneId, cut: StopCut, kind: CutKind, out: &mut Output) {
        let Some(at) = self.live(scene) else {
            return;
        };
        if kind == CutKind::Failed {
            self.end_calls_in(scene, out);
        }
        let held = &mut self.scenes[at];
        let logged = held.state.recovery_log().len();
        if held.state.stop(&cut, &mut self.global).is_err() {
            out.effects
                .push(Effect::Broken("a scene cut the brain can't apply"));
            return;
        }
        let revision = held.state.revision();
        if held.state.recovery_log().len() == logged {
            out.effects.push(Effect::Data(DataToShell::SceneCutApplied {
                scene,
                revision,
                cut,
            }));
            return;
        }
        let write = Write::Cut {
            scene,
            revision,
            cut: cut.clone(),
        };
        self.write(
            Wait::Cut {
                scene,
                revision,
                cut,
            },
            write,
            out,
        );
    }

    /// A stopped run's scenes that nothing in them ever activated and the shell sent no cut
    /// for, such as one whose first step was still being written, are cut to nothing, oldest
    /// first, each through the ordinary cut path, ahead of anything queued after the Stop. One
    /// at a time, so the queue never grows while it drains.
    pub(super) fn unseen(&mut self, run: RunId, mut named: Vec<SceneId>, out: &mut Output) {
        let Some(scene) = self
            .scenes
            .iter()
            .find(|held| {
                !held.gone
                    && !named.contains(&held.scene)
                    && held.state.activated() == 0
                    && held.committers.contains(&Some(run))
            })
            .map(|held| held.scene)
        else {
            return;
        };
        named.push(scene);
        self.queue.push_front(Job::Unseen { run, named });
        let cut = StopCut {
            activated: 0,
            revealed: Bounded::default(),
        };
        self.cut(scene, cut, CutKind::Stop, out);
    }

    /// A cut is durable: the shell may release its hold. A cut to nothing also leaves the
    /// visible set.
    pub(super) fn cut_stored(
        &mut self,
        scene: SceneId,
        revision: u32,
        cut: StopCut,
        out: &mut Output,
    ) {
        let Some(at) = self.live(scene) else {
            return;
        };
        out.effects.push(Effect::Data(DataToShell::SceneCutApplied {
            scene,
            revision,
            cut,
        }));
        if self.scenes[at].state.is_cleared() {
            self.scenes[at].gone = true;
            self.clear_stored(vec![scene], out);
        }
    }

    /// A step was first painted: its removals stop being charged.
    pub(super) fn shown(&mut self, scene: SceneId, index: u32, out: &mut Output) {
        let Some(at) = self.live(scene) else {
            return;
        };
        if self.scenes[at]
            .state
            .retire(index, &mut self.global)
            .is_err()
        {
            out.effects
                .push(Effect::Broken("a shown step the brain can't retire"));
        }
    }

    /// A new scene's first step makes room: the oldest of four visible scenes leaves now, in a
    /// write of its own ahead of the step's, so the shell drops it before the new one begins.
    pub(super) fn evict(&mut self, out: &mut Output) {
        if self.scenes.len() < MAX_VISIBLE_SCENES {
            return;
        }
        let mut oldest = self.scenes.remove(0);
        oldest
            .state
            .clear(&mut self.global)
            .expect("visible scenes share the one budget");
        let scenes = vec![oldest.scene];
        out.effects.push(Effect::RevokeScenes {
            scenes: scenes.clone(),
        });
        out.effects.push(Effect::Data(DataToShell::ScenesRemoved {
            scenes: scenes.clone(),
        }));
        self.clear_stored(scenes, out);
    }

    /// Writes a clear, which every new scene's first step then waits for.
    fn clear_stored(&mut self, scenes: Vec<SceneId>, out: &mut Output) {
        self.clearing = true;
        out.effects
            .push(Effect::Store(StoreOp::ClearScenes { scenes }));
    }

    /// The user cleared scenes: their unsent frames go and their clear is written now; their
    /// charges are released once no write is pending.
    pub(super) fn clear(&mut self, scenes: &[SceneId], out: &mut Output) {
        let left = self.leave(|held| scenes.contains(&held.scene));
        if left.is_empty() {
            return;
        }
        out.effects.push(Effect::RevokeScenes {
            scenes: left.clone(),
        });
        self.clear_stored(left, out);
    }

    /// Quit: every scene is cleared now, superseding any cut owed for it.
    pub(super) fn clear_visible(&mut self, out: &mut Output) {
        let left = self.leave(|_| true);
        if !left.is_empty() {
            self.clear_stored(left, out);
        }
    }

    /// The thread's rows are gone: its scenes leave without a clear, and a step of it still in
    /// the store changes nothing when answered.
    pub(super) fn deleted(&mut self, thread: ThreadId, out: &mut Output) {
        if let Some(Pending {
            wait: Wait::Step(write),
            ..
        }) = &mut self.pending
            && write.thread == thread
        {
            write.fenced = true;
        }
        let left = self.leave(|held| held.thread == thread);
        if left.is_empty() {
            return;
        }
        out.effects.push(Effect::RevokeScenes {
            scenes: left.clone(),
        });
        out.effects
            .push(Effect::Data(DataToShell::ScenesRemoved { scenes: left }));
    }

    /// Marks scenes as gone, so none of their frames goes out again. An owed cut of one is
    /// moot: the scene's clear supersedes it.
    fn leave(&mut self, which: impl Fn(&Held) -> bool) -> Vec<SceneId> {
        let mut left = Vec::new();
        for held in &mut self.scenes {
            if !held.gone && which(held) {
                held.gone = true;
                left.push(held.scene);
            }
        }
        if let Some(Pending {
            in_flight: false,
            wait: Wait::Cut { scene, .. },
            ..
        }) = &self.pending
            && left.contains(scene)
        {
            self.pending = None;
        }
        left
    }

    /// Releases the charges of scenes that left. Only while no write is pending, which their
    /// release would make stale.
    pub(super) fn sweep(&mut self) {
        let global = &mut self.global;
        self.scenes.retain_mut(|held| {
            if held.gone {
                held.state
                    .clear(global)
                    .expect("visible scenes share the one budget");
            }
            !held.gone
        });
    }
}
