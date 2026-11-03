//! The work the scheduler hands a background executor, and the answers it gets back.
//!
//! Cancellation is cooperative: a job observes its flag between preparation phases. Nothing here
//! kills a worker, so a cancelled or late job still answers, and the scheduler drops the answer.
//! A job, then its answer, holds one worker slot until the answer is applied or dropped, so the
//! cap also bounds finished results still waiting on the UI thread.

use std::{
    fmt,
    sync::{Arc, atomic::AtomicBool},
};

use kumochi_protocol::message::SceneId;
use kumochi_scene::{
    GlobalBudget,
    display::TextMeasure,
    layout::{self, PrepareError, PrepareInput, PreparedStep, SceneRecovery, SurfaceGeometry},
    theme::Theme,
};

use super::snapshot::Source;

/// One committed step's preparation on its scene's current tip.
pub(crate) struct PrepareJob {
    id: u64,
    scene: SceneId,
    input: PrepareInput,
    cancel: Arc<AtomicBool>,
    slot: Arc<()>,
}

/// One connection's recovered batch: every scene in one call, under one budget.
pub(crate) struct RecoveryJob {
    id: u64,
    sources: Vec<Source>,
    surface: SurfaceGeometry,
    theme: Arc<Theme>,
    cancel: Arc<AtomicBool>,
    slot: Arc<()>,
}

/// A job's answer, sent back to [`super::Presenter::completed`] whether or not it is still wanted.
pub(crate) enum Completion {
    /// A step's preparation.
    Prepared {
        id: u64,
        scene: SceneId,
        result: Result<PreparedStep, PrepareError>,
        slot: Arc<()>,
    },
    /// A recovery batch, one result per scene, with the budget the recovered scenes are bound to.
    Recovered {
        id: u64,
        scenes: Vec<(SceneId, SceneRecovery)>,
        global: GlobalBudget,
        slot: Arc<()>,
    },
}

impl PrepareJob {
    pub(super) fn new(
        id: u64,
        scene: SceneId,
        input: PrepareInput,
        cancel: Arc<AtomicBool>,
        slot: Arc<()>,
    ) -> Self {
        Self {
            id,
            scene,
            input,
            cancel,
            slot,
        }
    }

    /// Prepares on the calling worker thread until done or cancelled.
    pub(crate) fn run(self, measure: &mut dyn TextMeasure) -> Completion {
        Completion::Prepared {
            id: self.id,
            scene: self.scene,
            result: layout::prepare(self.input, measure, &self.cancel),
            slot: self.slot,
        }
    }

    #[cfg(test)]
    pub(super) fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn scene(&self) -> SceneId {
        self.scene
    }
}

impl RecoveryJob {
    pub(super) fn new(
        id: u64,
        sources: Vec<Source>,
        surface: SurfaceGeometry,
        theme: Arc<Theme>,
        cancel: Arc<AtomicBool>,
        slot: Arc<()>,
    ) -> Self {
        Self {
            id,
            sources,
            surface,
            theme,
            cancel,
            slot,
        }
    }

    /// Recovers the whole batch on the calling worker thread until done or cancelled.
    pub(crate) fn run(self, measure: &mut dyn TextMeasure) -> Completion {
        let batch: Vec<_> = self
            .sources
            .iter()
            .map(|source| (&source.header.begin, source.log.as_slice()))
            .collect();
        let (results, global) =
            layout::recover_all(&batch, self.surface, &self.theme, measure, &self.cancel);
        let scenes = self
            .sources
            .iter()
            .map(|source| source.scene)
            .zip(results)
            .collect();
        Completion::Recovered {
            id: self.id,
            scenes,
            global,
            slot: self.slot,
        }
    }

    #[cfg(test)]
    pub(super) fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl fmt::Debug for PrepareJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrepareJob")
            .field("id", &self.id)
            .field("scene", &self.scene)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for RecoveryJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryJob")
            .field("id", &self.id)
            .field("scenes", &self.sources.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Completion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepared {
                id, scene, result, ..
            } => f
                .debug_struct("Prepared")
                .field("id", id)
                .field("scene", scene)
                .field("ok", &result.is_ok())
                .finish(),
            Self::Recovered { id, scenes, .. } => f
                .debug_struct("Recovered")
                .field("id", id)
                .field("scenes", &scenes.len())
                .finish_non_exhaustive(),
        }
    }
}
