use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use kumochi_protocol::message::{ControlToBrain, MAX_VISIBLE_SCENES, RunId, SceneId};
use kumochi_scene::{
    Preparation, SceneEvent, SceneState, StopCut,
    deferred::Deferred,
    layout::{LayoutState, PreparedStep},
    text::Bounded,
};

use super::Effect;

pub(super) struct Scene {
    pub(super) state: SceneState,
    /// The layout at the activated bound: recovered at rest, or empty for a fresh scene.
    pub(super) rest: LayoutState,
    pub(super) retained: usize,
    pub(super) queue: VecDeque<Ticket>,
    pub(super) running: Option<Running>,
    /// Prepared and not activated; nothing activates before B6.
    pub(super) ready: Vec<Deferred<PreparedStep>>,
    pub(super) cut: Cut,
    /// A commit went unprepared, so nothing prepares again until a cut resets the layout.
    pub(super) skipping: bool,
}

pub(super) struct Ticket {
    pub(super) run: RunId,
    pub(super) index: u32,
    /// When it could first start: its commit, or its predecessor's answer.
    pub(super) queued: Instant,
    pub(super) preparation: Deferred<Preparation>,
}

pub(super) struct Running {
    pub(super) id: u64,
    pub(super) run: RunId,
    pub(super) index: u32,
    pub(super) started: Instant,
    pub(super) cancel: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cut {
    /// No cut since the last commit.
    Open,
    /// A cut was sent and the brain hasn't acknowledged it.
    Outstanding,
    /// A cut was applied, and no step has committed since.
    Applied,
}

/// Effects in order, with the clears gathered into as few `ClearDrawing`s as possible.
#[derive(Default)]
pub(super) struct Out {
    pub(super) effects: Vec<Effect>,
    pub(super) clear: Vec<SceneId>,
}

impl Out {
    pub(super) fn credit(&mut self, bytes: u32) {
        if bytes > 0 {
            self.effects.push(Effect::Credit(bytes));
        }
    }

    pub(super) fn flush(&mut self) {
        self.clear.sort_unstable();
        self.clear.dedup();
        for scene_ids in self.clear.chunks(MAX_VISIBLE_SCENES) {
            let scene_ids = scene_ids.to_vec();
            self.effects
                .push(Effect::Control(ControlToBrain::ClearDrawing { scene_ids }));
        }
        self.clear.clear();
    }

    pub(super) fn finish(mut self) -> Vec<Effect> {
        self.flush();
        self.effects
    }
}

impl Scene {
    pub(super) fn new(state: SceneState, rest: LayoutState, retained: usize) -> Self {
        let cut = if matches!(state.recovery_log().last(), Some(SceneEvent::Stop(_))) {
            Cut::Applied
        } else {
            Cut::Open
        };
        Self {
            retained,
            state,
            rest,
            queue: VecDeque::new(),
            running: None,
            ready: Vec::new(),
            cut,
            skipping: false,
        }
    }

    /// The step the watchdog times, and since when: the running one, or else the next queued one,
    /// which only waits while every worker slot is taken.
    pub(super) fn due(&self) -> Option<(Instant, RunId, u32)> {
        match (&self.running, self.queue.front()) {
            (Some(running), _) => Some((running.started, running.run, running.index)),
            (None, Some(ticket)) => Some((ticket.queued, ticket.run, ticket.index)),
            (None, None) => None,
        }
    }

    /// The layout the next step prepares on.
    pub(super) fn tip(&self) -> &LayoutState {
        self.ready
            .last()
            .map_or(&self.rest, |prepared| &prepared.after)
    }

    pub(super) fn residency(&self) -> usize {
        self.ready.iter().fold(self.retained, |sum, prepared| {
            sum.saturating_add(prepared.resident_bytes)
        })
    }

    /// What the dropper is told this scene owns: its prepared layouts and its semantic state.
    pub(super) fn attributed(&self) -> usize {
        self.residency()
            .saturating_add(self.state.attributed_bytes())
    }

    /// Drops queued and prepared work, and asks a running job to stop.
    pub(super) fn halt(&mut self) {
        if let Some(running) = self.running.take() {
            running.cancel.store(true, Ordering::Relaxed);
        }
        drop(Deferred::new(
            std::mem::take(&mut self.queue),
            self.state.attributed_bytes(),
        ));
        let bytes = self.residency().saturating_sub(self.retained);
        drop(Deferred::new(std::mem::take(&mut self.ready), bytes));
    }

    /// The cut at the activated bound, keeping every object at rest there.
    pub(super) fn cut_now(&self) -> StopCut {
        StopCut {
            activated: self.state.activated(),
            revealed: Bounded::new(self.rest.revealed_at_rest())
                .expect("a scene holds at most its object cap"),
        }
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
