//! Visible scenes: commits answered by the store, cuts, retirements, clears, deletions and
//! fifth-scene eviction.

use kumochi_protocol::message::{
    DataToShell, MAX_VISIBLE_SCENES, RunId, SceneCuts, SceneFailure, SceneHeader, SceneId, ThreadId,
};
use kumochi_scene::layout::LayoutState;
use kumochi_scene::record::Surface;
use kumochi_scene::text::Bounded;
use kumochi_scene::{ReplacementDisposition, Step, StopCut};

use super::Author;
use super::io::Write;
use super::{
    CutKind, Drawing, Held, Job, LAYOUT_VERSION, Output, Paint, Pend, Pending, StepWait, Wait,
};
use crate::coordinator::{Effect, StoreOp};

impl Drawing {
    /// A replacement that showed nothing failed. Its run owns the loss, so a corrected call that
    /// paints can still drop it.
    pub(super) fn replacement_failed(&mut self, scene: SceneId, out: &mut Output) {
        let Some(at) = self.live(scene).filter(|&at| self.scenes[at].replaced) else {
            return;
        };
        let held = &mut self.scenes[at];
        held.replaced = false;
        let (run, thread) = (
            held.committers[0].expect("replacement's first step was committed live"),
            held.thread,
        );
        if run <= self.closed {
            out.effects.push(Effect::Control(
                kumochi_protocol::message::ControlToShell::DrawingReplacementFailed { thread, run },
            ));
            self.end_loss(run, super::Loss::Drop, out);
            return;
        }
        self.ledger(run).loss = Some(thread);
        self.settle_loss(run, out);
    }

    /// Tells the shell of `run`'s loss once the run can no longer draw a step.
    pub(super) fn settle_loss(&mut self, run: RunId, out: &mut Output) {
        if self
            .runs
            .iter()
            .any(|ledger| ledger.run == run && ledger.loss.is_some())
            && self.remaining(run) != super::Remaining::Steps
        {
            self.end_loss(run, super::Loss::Tell, out);
        }
    }

    /// Ends `run`'s loss, telling the shell unless it's dropped; none of its scenes reports one
    /// again.
    pub(super) fn end_loss(&mut self, run: RunId, loss: super::Loss, out: &mut Output) {
        if let Some(ledger) = self.runs.iter_mut().find(|ledger| ledger.run == run)
            && let Some(thread) = ledger.loss.take()
            && loss == super::Loss::Tell
        {
            ledger.loss_told = true;
            out.effects.push(Effect::Control(
                kumochi_protocol::message::ControlToShell::DrawingReplacementFailed { thread, run },
            ));
        }
        if let Some(Pending {
            wait: Wait::Step(write) | Wait::Prepare { step: write, .. },
            ..
        }) = &mut self.pending
            && write.run == run
        {
            write.loss_suppressed = true;
        }
        for held in &mut self.scenes {
            if held.committers.first() == Some(&Some(run)) {
                held.replaced = false;
            }
        }
    }

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
    #[expect(
        clippy::too_many_lines,
        reason = "one committed write keeps victim retirement and successor installation ordered"
    )]
    pub(super) fn stored(&mut self, write: StepWait, id: SceneId, out: &mut Output) {
        let StepWait {
            run,
            call,
            thread,
            scene,
            step,
            fenced,
            clear_requested,
            loss_suppressed,
            interrupted,
            laid,
            author,
        } = write;
        let begun = matches!(scene, Pend::New { .. });
        let (scene, index) = match scene {
            Pend::Held { scene, admitted } => {
                let Some(at) = self.live(scene) else {
                    // Cleared meanwhile, but the step is durable, so it spends the run's budget.
                    self.drew(run, &call, scene, &step);
                    if let Some(error) = interrupted {
                        self.finish(run, &call, Some(error), out);
                    }
                    return;
                };
                if fenced {
                    return;
                }
                let held = &mut self.scenes[at];
                held.author = held.author.clone().and(author);
                let committed = held
                    .state
                    .commit(admitted, &mut self.global)
                    .expect("the pending write sealed its scene and the global budget");
                held.committers.push(Some(run));
                held.paint = held.paint.take().zip(laid).map(|(mut paint, laid)| {
                    paint.committed(committed.index, laid);
                    paint
                });
                (scene, committed.index)
            }
            Pend::New { admission, victims } => {
                let replaced = !victims.is_empty() && !loss_suppressed;
                let mut states: Vec<_> = self
                    .scenes
                    .iter_mut()
                    .filter(|held| victims.contains(&held.scene))
                    .map(|held| &mut held.state)
                    .collect();
                let disposition = if fenced || clear_requested {
                    ReplacementDisposition::Discard
                } else {
                    ReplacementDisposition::Install
                };
                let installed = (*admission)
                    .settle(&mut states, &mut self.global, disposition)
                    .expect("the pending replacement sealed every victim and the global budget");
                let mut retiring = victims;
                self.scenes.retain(|held| {
                    let Some(at) = retiring.iter().position(|scene| *scene == held.scene) else {
                        return true;
                    };
                    if held.gone {
                        retiring.remove(at);
                    }
                    false
                });
                if !retiring.is_empty() {
                    out.effects.push(Effect::RetireScenes {
                        scenes: retiring,
                        successor: installed.is_some().then_some(id),
                    });
                }
                let Some((state, committed)) = installed else {
                    self.drew(run, &call, id, &step);
                    if clear_requested {
                        self.clear_stored(vec![id], out);
                    }
                    self.finish(
                        run,
                        &call,
                        Some(interrupted.unwrap_or(super::CallError::Interrupted)),
                        out,
                    );
                    return;
                };
                let header = SceneHeader {
                    begin: state.header().clone(),
                    layout_version: LAYOUT_VERSION,
                };
                out.effects
                    .push(Effect::Data(DataToShell::SceneBegin { scene: id, header }));
                let paint =
                    laid.map(|laid| Paint::begun(LayoutState::new(&state), committed.index, laid));
                let author = if !author.is_protected() && state.header().surface == Surface::Screen
                {
                    Author::Unknown
                } else {
                    Author::Public.and(author)
                };
                self.scenes.push(Held {
                    scene: id,
                    thread,
                    layout_version: LAYOUT_VERSION,
                    state,
                    author,
                    committers: vec![Some(run)],
                    refused_through: 0,
                    gone: false,
                    replaced,
                    paint,
                });
                (id, committed.index)
            }
        };
        out.effects.push(Effect::Data(DataToShell::SceneStepCommit {
            run,
            scene,
            index,
            step: Step::clone(&step),
            speech: None,
        }));
        self.drew(run, &call, scene, &step);
        if begun {
            self.began(thread, scene, out);
        }
        if let Some(error) = interrupted {
            self.finish(run, &call, Some(error), out);
        }
    }

    pub(super) fn stop_scenes(&mut self, run: RunId, cuts: &SceneCuts, out: &mut Output) {
        // shortcut: D1 has one run; OSC-56 needs per-run revocation for concurrency.
        for cut in cuts.iter() {
            let job = Job::Cut {
                scene: cut.scene,
                cut: cut.cut.clone(),
                kind: CutKind::Stop,
            };
            self.enqueue(job, out);
        }
        let named = cuts.iter().map(|cut| cut.scene).collect();
        self.enqueue(Job::Unseen { run, named }, out);
    }

    /// Applies a scene's cut, whichever runs committed what it drops. Every effective cut is
    /// durable before the shell hears it applied; one that changes nothing is acknowledged at
    /// once. A cut the scene rejects can only come from a broken shell.
    pub(super) fn cut(&mut self, scene: SceneId, cut: StopCut, kind: CutKind, out: &mut Output) {
        let Some(at) = self.live(scene) else {
            return;
        };
        if let CutKind::Failed(reason) = kind {
            let held = &mut self.scenes[at];
            let from = usize::try_from(cut.activated)
                .unwrap_or(usize::MAX)
                .max(held.refused_through);
            for run in held.committers.iter().skip(from).flatten() {
                if let Some(ledger) = self.runs.iter_mut().find(|ledger| ledger.run == *run) {
                    ledger.refuse_step();
                }
            }
            held.refused_through = held.committers.len();
            self.end_calls_in(scene, &failure(reason, &cut), out);
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
            self.cut_applied(at);
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
        self.cut_applied(at);
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

    /// The shell hears the cut as its `SceneCutApplied` goes out and keeps only the cut layout
    /// from then on, so the scene's charge stays the pre-cut one until that moment.
    fn cut_applied(&mut self, at: usize) {
        let held = &mut self.scenes[at];
        held.paint = held.paint.take().and_then(|paint| paint.cut(&held.state));
    }

    /// A step was first painted: its removals stop being charged.
    pub(super) fn shown(&mut self, scene: SceneId, index: u32, out: &mut Output) {
        let Some(at) = self.live(scene) else {
            return;
        };
        if let Some(paint) = &mut self.scenes[at].paint {
            paint.shown(index);
        }
        if self.scenes[at]
            .state
            .retire(index, &mut self.global)
            .is_err()
        {
            out.effects
                .push(Effect::Broken("a shown step the brain can't retire"));
        }
        self.answered(scene, index, out);
    }

    pub(super) fn replacement_victims(
        &self,
        thread: ThreadId,
        others: kumochi_scene::Others,
    ) -> Vec<SceneId> {
        let replaces =
            |held: &&Held| others == kumochi_scene::Others::Clear && held.thread == thread;
        let survivors: Vec<_> = self
            .scenes
            .iter()
            .filter(|held| !held.gone)
            .filter(|held| !replaces(held))
            .collect();
        let evict = (survivors.len() >= MAX_VISIBLE_SCENES).then(|| survivors[0].scene);
        self.scenes
            .iter()
            .filter(|held| !held.gone)
            .filter(|held| replaces(held) || Some(held.scene) == evict)
            .map(|held| held.scene)
            .collect()
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
        self.clear_newcomers(out);
        self.clear_scenes(scenes, out);
    }

    pub(super) fn scene_failed(&mut self, scene: SceneId, out: &mut Output) {
        if self.live(scene).is_none() {
            return;
        }
        self.replacement_failed(scene, out);
        self.end_calls_in(scene, &super::CallError::Failed, out);
        self.clear_scenes(&[scene], out);
    }

    pub(super) fn clear_scenes(&mut self, scenes: &[SceneId], out: &mut Output) {
        let left = self.leave(|held| scenes.contains(&held.scene));
        if left.is_empty() {
            return;
        }
        out.effects.push(Effect::RetireScenes {
            scenes: left.clone(),
            successor: None,
        });
        self.clear_stored(left, out);
    }

    /// Quit: every scene is cleared now, superseding any cut owed for it.
    pub(super) fn clear_visible(&mut self, out: &mut Output) {
        self.clear_newcomers(out);
        let left = self.leave(|_| true);
        if !left.is_empty() {
            self.clear_stored(left, out);
        }
    }

    /// The thread's rows are gone: its scenes leave without a clear, and a step of it still in
    /// the store suppresses its newcomer but still settles committed replacement victims.
    pub(super) fn deleted(&mut self, thread: ThreadId, out: &mut Output) {
        for ledger in &mut self.runs {
            if ledger.loss == Some(thread) {
                ledger.loss = None;
            }
        }
        self.cancel_preparation(|step| step.thread == thread);
        if let Some(Pending {
            wait: Wait::Step(write) | Wait::Prepare { step: write, .. },
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
        out.effects.push(Effect::RetireScenes {
            scenes: left,
            successor: None,
        });
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
        self.cancel_preparation(
            |step| matches!(step.scene, Pend::Held { scene, .. } if left.contains(&scene)),
        );
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

/// What a call drawing in a scene hears when the shell couldn't prepare a step there.
pub(super) fn failure(reason: SceneFailure, cut: &StopCut) -> super::CallError {
    if reason == SceneFailure::PaintWork {
        super::CallError::TooMuchToPaint {
            cleared: cut.activated == 0,
        }
    } else {
        super::CallError::Failed
    }
}
