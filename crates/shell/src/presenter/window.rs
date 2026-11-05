//! Consumption of a scene's oldest prepared step, for the activation owner B6 adds.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use kumochi_protocol::message::SceneId;
use kumochi_scene::{deferred::Deferred, layout::PreparedStep, limits::MAX_RESIDENT_BYTES};

use super::{Broken, Effect, Out, Presenter, Ready};

/// Why nothing was consumed; each leaves the presenter unchanged, except that `Busy` keeps the
/// request.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "B6's activation owner is the production caller")
)]
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
    /// Its layout as the rest, with this owner's charge, would pass the prepared allowance.
    Residency,
    /// The scene's admission is out. The request is kept, and `Effect::ConsumptionReady` names
    /// it once the admission is in; the scene admits nothing more until it is served or halts.
    Busy,
}

/// A consumed step, charged against the prepared allowance until its owner drops it.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "B6's activation owner is the production caller")
)]
#[derive(Debug)]
pub(crate) struct ConsumedStep {
    step: Deferred<PreparedStep>,
    bytes: usize,
    returned: Arc<AtomicUsize>,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "B6's activation owner is the production caller")
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

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "B6's activation owner is the production caller")
)]
#[derive(Debug)]
pub(crate) struct Consumed {
    pub(crate) step: ConsumedStep,
    /// The steps its freed window position starts.
    pub(crate) effects: Result<Vec<Effect>, Broken>,
}

impl Presenter {
    /// Consumes the scene's oldest prepared step, which must be `expected_index`: its removals
    /// retire, its layout becomes the rest, its window position frees and held frames resume.
    /// # Errors
    /// [`ConsumeError`], with nothing changed.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "B6's activation owner is the production caller")
    )]
    pub(crate) fn consume_ready(
        &mut self,
        id: SceneId,
        expected_index: u32,
        now: Instant,
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
        let bytes = head.step.attribution();
        let after = total
            .saturating_sub(scene.retained.saturating_add(head.step.resident_bytes))
            .saturating_add(head.full_after)
            .saturating_add(bytes);
        if after > MAX_RESIDENT_BYTES {
            return Err(ConsumeError::Residency);
        }
        scene
            .state
            .retire(expected_index, &mut self.global)
            .expect("a prepared step was committed under this budget, and no admission reads it");
        let Ready { step, full_after } = scene.ready.pop_front().expect("the head was just read");
        // shortcut: test-only consumption keeps its old view; replace and charge it atomically when B6 wires activation
        let old = std::mem::replace(&mut scene.rest, step.after.clone());
        drop(Deferred::new(old, scene.retained));
        scene.retained = full_after;
        scene.pending_consumption = None;
        self.returned.fetch_add(bytes, Ordering::Relaxed);
        let step = ConsumedStep {
            step,
            bytes,
            returned: Arc::clone(&self.returned),
        };
        let mut out = Out::default();
        let effects = self.pump_all(now, &mut out).map(|()| out.finish());
        Ok(Consumed { step, effects })
    }
}
