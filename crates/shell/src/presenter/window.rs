//! Activation of a scene's oldest prepared step, for the activation owner B6d adds.
//!
//! One call stages the head against the scene's view, and its paint cache, checks the whole
//! post-transition charge, then retires the semantic step and installs the rest, the view, its
//! cache and a fresh frame epoch together. A refusal changes nothing, and anything staged for it
//! goes to the dropper.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use kumochi_protocol::message::{RunId, SceneId};
use kumochi_scene::{deferred::Deferred, layout::PreparedStep, limits::MAX_RESIDENT_BYTES};

use super::{Broken, Effect, Out, Presenter, Ready};

/// Why nothing was consumed; each leaves the presenter unchanged, except that `Busy` keeps the
/// request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConsumeError {
    /// No such scene is mirrored.
    UnknownScene,
    /// That step was consumed already.
    AlreadyConsumed,
    /// Another step is the oldest prepared one.
    OutOfOrder,
    /// No step is prepared yet.
    NotReady,
    /// The head was not prepared on the scene's current semantic state, rest and view.
    Stale,
    /// Its rest and activated view, with this owner's charge, would pass the prepared allowance.
    Residency,
    /// The scene's admission is out. The request is kept, and `Effect::ConsumptionReady` names
    /// it once the admission is in; the scene admits nothing more until it is served or halts.
    Busy,
}

/// A consumed step, charged against the prepared allowance until its owner drops it.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "B6d's activation owner is the production caller")
)]
#[derive(Debug)]
pub(crate) struct ConsumedStep {
    step: Deferred<PreparedStep>,
    bytes: usize,
    returned: Arc<AtomicUsize>,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "B6d's activation owner is the production caller")
)]
impl ConsumedStep {
    pub(crate) fn step(&self) -> &PreparedStep {
        &self.step
    }
}

impl Drop for ConsumedStep {
    fn drop(&mut self) {
        self.returned.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub(crate) struct Consumed {
    pub(crate) step: ConsumedStep,
    pub(crate) run: RunId,
    pub(crate) epoch: u64,
    /// The steps its freed window position starts.
    pub(crate) effects: Result<Vec<Effect>, Broken>,
}

impl Presenter {
    /// Activates the scene's oldest prepared step, which must be `expected_index`, at
    /// `scene_time` over a narration segment of `segment_ms`: its removals retire, its layout
    /// becomes the rest, its activated view replaces the old one in a new frame epoch, its
    /// window position frees and held frames resume. The step ends at the later of the
    /// segment's end and its last entrance's.
    /// # Errors
    /// [`ConsumeError`], with nothing changed.
    pub(crate) fn consume_ready(
        &mut self,
        id: SceneId,
        expected_index: u32,
        now: Instant,
        scene_time: u64,
        segment_ms: u64,
    ) -> Result<Consumed, ConsumeError> {
        let busy = self.lane.as_ref().is_some_and(|lane| lane.scene == id);
        let total = self.residency();
        let scene = self.scenes.get_mut(&id).ok_or(ConsumeError::UnknownScene)?;
        if expected_index < scene.state.view().activated() {
            return Err(ConsumeError::AlreadyConsumed);
        }
        let head = scene.ready.front().ok_or(ConsumeError::NotReady)?;
        if head.step.index != expected_index {
            return Err(ConsumeError::OutOfOrder);
        }
        if busy {
            scene.pending_consumption = Some(expected_index);
            return Err(ConsumeError::Busy);
        }
        head.step
            .validate(scene.state.view(), &scene.rest)
            .map_err(|_| ConsumeError::Stale)?;
        let activation = scene
            .view
            .prepare_activation(&head.step, scene_time, segment_ms)
            .map_err(|_| ConsumeError::Stale)?;
        let segment_ms = activation.view().entrance_end().map_or(segment_ms, |end| {
            segment_ms.max(end.saturating_sub(scene_time))
        });
        let cache = self.stage.stage(activation.view());
        let cache_bytes = cache.bytes();
        let bytes = head.step.attribution();
        let view_bytes = activation.resident_bytes();
        let outgoing = scene
            .retained
            .saturating_add(scene.view_bytes)
            .saturating_add(scene.cache_bytes)
            .saturating_add(head.step.resident_bytes);
        let mut after = total
            .saturating_sub(outgoing)
            .saturating_add(head.full_after)
            .saturating_add(view_bytes)
            .saturating_add(cache_bytes)
            .saturating_add(bytes);
        if after > MAX_RESIDENT_BYTES {
            let mut frames = self.frames.borrow_mut();
            after = after.saturating_sub(frames.retirement_bytes());
            frames.discard_retirement();
        }
        if after > MAX_RESIDENT_BYTES {
            drop(Deferred::new(cache, cache_bytes));
            return Err(ConsumeError::Residency);
        }
        scene
            .state
            .retire(expected_index, &mut self.global)
            .expect("a prepared step was committed under this budget, and no admission reads it");
        let Ready {
            run,
            step,
            full_after,
        } = scene.ready.pop_front().expect("the head was just read");
        let old = std::mem::replace(&mut scene.rest, step.after.clone());
        drop(Deferred::new(old, scene.retained));
        scene.retained = full_after;
        let old = self
            .frames
            .borrow_mut()
            .activate(id, &mut scene.view, activation, cache);
        drop(Deferred::new(old, scene.cache_bytes));
        let epoch = self
            .frames
            .borrow()
            .epoch(id)
            .expect("an installed frame has an epoch");
        scene.activation.install(
            self.generation.expect("a scene belongs to a connection"),
            run,
            expected_index,
            scene_time,
            segment_ms,
        );
        scene.view_bytes = view_bytes;
        scene.cache_bytes = cache_bytes;
        scene.paint = scene
            .ready
            .iter()
            .fold(scene.rest.paint_work(), |sum, ready| {
                sum.max(ready.step.after.paint_work())
            });
        scene.pending_consumption = None;
        self.returned.fetch_add(bytes, Ordering::Relaxed);
        let step = ConsumedStep {
            step,
            bytes,
            returned: Arc::clone(&self.returned),
        };
        let mut out = Out::default();
        let effects = self.pump_all(now, &mut out).map(|()| out.finish());
        Ok(Consumed {
            step,
            run,
            epoch,
            effects,
        })
    }
}
