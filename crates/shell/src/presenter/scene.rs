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

/// Read-ahead positions per scene: prepared results not yet consumed, plus the running one.
pub(super) const WINDOW: usize = 8;

pub(super) struct Scene {
    pub(super) state: SceneState,
    /// The layout at the activated bound: recovered at rest, empty, or the last consumed step's.
    pub(super) rest: LayoutState,
    pub(super) retained: usize,
    pub(super) queue: VecDeque<Ticket>,
    pub(super) running: Option<Running>,
    /// Prepared and not consumed, oldest first.
    pub(super) ready: VecDeque<Ready>,
    pub(super) cut: Cut,
    /// A commit went unprepared, so nothing prepares again until a cut resets the layout.
    pub(super) skipping: bool,
}

/// A prepared result, with the worker's full charge of its layout for when it becomes rest.
pub(super) struct Ready {
    pub(super) step: Deferred<PreparedStep>,
    pub(super) full_after: usize,
}

pub(super) struct Ticket {
    pub(super) run: RunId,
    pub(super) index: u32,
    /// Stamped once, when its predecessor is ready and the window has room; untimed until then.
    pub(super) eligible_at: Option<Instant>,
    pub(super) preparation: Deferred<Preparation>,
}

pub(super) struct Running {
    pub(super) id: u64,
    pub(super) run: RunId,
    pub(super) index: u32,
    pub(super) eligible_at: Instant,
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
            ready: VecDeque::new(),
            cut,
            skipping: false,
        }
    }

    /// The step the watchdog times, and since when it was eligible: the running one, or else the
    /// next queued one once eligible, which then only waits for a worker slot.
    pub(super) fn due(&self) -> Option<(Instant, RunId, u32)> {
        match (&self.running, self.queue.front()) {
            (Some(running), _) => Some((running.eligible_at, running.run, running.index)),
            (None, Some(ticket)) => Some((ticket.eligible_at?, ticket.run, ticket.index)),
            (None, None) => None,
        }
    }

    /// Whether the next step's predecessor is ready and the window has a position for it.
    pub(super) fn open(&self) -> bool {
        self.running.is_none() && self.ready.len() < WINDOW
    }

    /// The layout the next step prepares on.
    pub(super) fn tip(&self) -> &LayoutState {
        self.ready
            .back()
            .map_or(&self.rest, |ready| &ready.step.after)
    }

    pub(super) fn residency(&self) -> usize {
        self.ready.iter().fold(self.retained, |sum, ready| {
            sum.saturating_add(ready.step.resident_bytes)
        })
    }

    /// What the dropper is told this scene owns: its prepared layouts and its semantic state.
    pub(super) fn attributed(&self) -> usize {
        self.residency()
            .saturating_add(self.state.attributed_bytes())
    }

    /// Drops queued and prepared work, and asks a running job to stop. Its window positions free
    /// here, once; a late answer finds nothing running.
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
