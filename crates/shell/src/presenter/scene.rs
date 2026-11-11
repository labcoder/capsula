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
    Frame, Preparation, SceneBinding, SceneEvent, SceneView, StopCut,
    deferred::Deferred,
    display::PaintWork,
    layout::{LayoutState, PreparedStep},
    text::Bounded,
};

use super::{Activation, Effect, Presenter, SceneCache};
use kumochi_protocol::message::SceneId;

impl Presenter {
    /// Adopts a bound scene and reserves its frame, with the cache staged from its view.
    #[expect(
        clippy::too_many_arguments,
        reason = "adoption moves every owner of the new scene into place at once"
    )]
    pub(super) fn install(
        &mut self,
        id: SceneId,
        state: SceneBinding,
        rest: LayoutState,
        retained: usize,
        view: SceneView,
        frame: Frame,
        cache: Box<dyn SceneCache>,
        activation: Activation,
    ) {
        let cache_bytes = cache.bytes();
        let frame = self.frames.borrow_mut().adopt(id, frame, cache);
        let mut scene = Scene::new(state, rest, retained, view, frame, cache_bytes, activation);
        if let Some(pending) = self
            .stopped_begins
            .iter_mut()
            .find(|slot| **slot == Some(id))
        {
            *pending = None;
            scene.cut = Cut::Outstanding;
            scene.skipping = true;
        }
        self.scenes.insert(id, scene);
    }
}

/// Read-ahead positions per scene: prepared results not yet consumed, plus the running one.
pub(super) const WINDOW: usize = 8;

pub(super) struct Scene {
    pub(super) activation: Activation,
    pub(super) state: SceneBinding,
    /// The layout at the activated bound: recovered at rest, empty, or the last consumed step's.
    pub(super) rest: LayoutState,
    pub(super) retained: usize,
    pub(super) paint: PaintWork,
    /// The sampled source of the rest: an activated view installed with it, or at rest.
    pub(super) view: SceneView,
    /// The view's bytes, cached when it is replaced.
    pub(super) view_bytes: usize,
    /// The frame reserved for this scene in the presenter's [`super::Frames`].
    pub(super) frame_bytes: usize,
    /// The paint cache beside that frame, as its stage charged it.
    pub(super) cache_bytes: usize,
    pub(super) queue: VecDeque<Ticket>,
    pub(super) running: Option<Running>,
    /// Prepared and not consumed, oldest first.
    pub(super) ready: VecDeque<Ready>,
    /// A consumption refused while this scene's admission was out, served before its next one.
    pub(super) pending_consumption: Option<u32>,
    pub(super) cut: Cut,
    /// The scene time of this view's last sample, which a Stop latches.
    pub(super) sampled_ms: Option<u64>,
    /// The sample a sent cut latched when it started; its acknowledgement settles at this time,
    /// however many samples arrive before it.
    pub(super) stopped_ms: Option<u64>,
    /// A commit went unprepared, so nothing prepares again until a cut resets the layout.
    pub(super) skipping: bool,
}

/// A prepared result, with the worker's full charge of its layout for when it becomes rest.
pub(super) struct Ready {
    pub(super) run: RunId,
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
    pub(super) fn new(
        state: SceneBinding,
        rest: LayoutState,
        retained: usize,
        view: SceneView,
        frame_bytes: usize,
        cache_bytes: usize,
        activation: Activation,
    ) -> Self {
        let cut = if matches!(
            state.view().recovery_log().last(),
            Some(SceneEvent::Stop(_))
        ) {
            Cut::Applied
        } else {
            Cut::Open
        };
        Self {
            activation,
            retained,
            paint: rest.paint_work(),
            view_bytes: view.resident_bytes(),
            view,
            frame_bytes,
            cache_bytes,
            state,
            rest,
            queue: VecDeque::new(),
            running: None,
            ready: VecDeque::new(),
            pending_consumption: None,
            cut,
            sampled_ms: None,
            stopped_ms: None,
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
        self.ready.iter().fold(self.held(), |sum, ready| {
            sum.saturating_add(ready.step.resident_bytes)
        })
    }

    /// What stays charged while its prepared work goes: the rest, its view, its frame and the
    /// frame's paint cache.
    pub(super) fn held(&self) -> usize {
        self.retained
            .saturating_add(self.view_bytes)
            .saturating_add(self.frame_bytes)
            .saturating_add(self.cache_bytes)
            .saturating_add(Activation::BYTES)
    }

    /// Drops queued and prepared work with any consumption waiting on it, and asks a running job
    /// to stop. Its window positions free here, once; a late answer finds nothing running.
    pub(super) fn halt(&mut self) {
        self.activation.halt();
        if let Some(running) = self.running.take() {
            running.cancel.store(true, Ordering::Relaxed);
        }
        let bytes = self.queue.capacity() * size_of::<Ticket>()
            + self
                .queue
                .iter()
                .map(|ticket| ticket.preparation.attribution())
                .sum::<usize>();
        drop(Deferred::new(std::mem::take(&mut self.queue), bytes));
        let bytes = self.ready.capacity() * size_of::<Ready>()
            + self
                .ready
                .iter()
                .map(|ready| ready.step.attribution())
                .sum::<usize>();
        drop(Deferred::new(std::mem::take(&mut self.ready), bytes));
        self.paint = self.rest.paint_work();
        self.pending_consumption = None;
    }

    /// The cut at the activated bound, keeping every object at rest there.
    pub(super) fn cut_now(&self) -> StopCut {
        StopCut {
            activated: self.state.view().activated(),
            revealed: Bounded::new(
                self.view
                    .stop(self.stopped_ms.or(self.sampled_ms).unwrap_or(0)),
            )
            .expect("a scene holds at most its object cap"),
        }
    }
}

impl std::fmt::Debug for Scene {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scene")
            .field("revision", &self.state.view().revision())
            .field("queued", &self.queue.len())
            .field("ready", &self.ready.len())
            .field("cut", &self.cut)
            .finish_non_exhaustive()
    }
}
