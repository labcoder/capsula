//! The work the scheduler hands a background executor, and the answers it gets back.
//!
//! Cancellation is cooperative: a job observes its flag between preparation phases. Nothing here
//! kills a worker, so a cancelled or late job still answers, and the scheduler drops the answer.
//! A job, then its answer, holds one worker slot until the answer is applied or dropped, so the
//! cap also bounds finished results still waiting on the UI thread.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use kumochi_protocol::message::{DataToShell, SceneId};
use kumochi_scene::{
    DetachedCandidate, GlobalBudget, SceneRead, StepError,
    deferred::Deferred,
    display::{PaintWork, TextMeasure},
    layout::{self, PrepareError, PrepareInput, PreparedStep, SceneRecovery, SurfaceGeometry},
    theme::Theme,
};

use super::snapshot::Source;

/// One committed step's preparation on its scene's current tip.
pub(crate) struct PrepareJob {
    id: u64,
    scene: SceneId,
    input: Deferred<PrepareInput>,
    cancel: Arc<AtomicBool>,
    slot: Arc<()>,
}

/// One connection's recovered batch: every scene in one call, under one budget.
pub(crate) struct RecoveryJob {
    id: u64,
    sources: Deferred<Vec<Source>>,
    surface: SurfaceGeometry,
    theme: Arc<Theme>,
    cancel: Arc<AtomicBool>,
    slot: Arc<()>,
}

/// One commit's admission against a read-only snapshot of its scene, installed through the
/// scene's binding when it comes back.
pub(crate) struct AdmitJob {
    id: u64,
    read: SceneRead,
    frame: Deferred<DataToShell>,
    cancel: Arc<AtomicBool>,
    slot: Arc<()>,
}

/// A job's answer, sent back to [`super::Presenter::completed`] whether or not it is still wanted.
/// Its payload drops on the dropper thread wherever the answer is discarded.
pub(crate) enum Completion {
    /// A step's preparation.
    Prepared {
        id: u64,
        scene: SceneId,
        result: Deferred<Result<PreparedStep, PrepareError>>,
        /// The full charge of the result's layout alone, for when it becomes rest.
        full_after: usize,
        slot: Arc<()>,
    },
    /// An admission with the snapshot it read, which the install gives back; none if cancelled.
    Admitted {
        id: u64,
        read: SceneRead,
        result: Option<Result<DetachedCandidate, StepError>>,
        slot: Arc<()>,
    },
    /// A recovery batch, one result per scene, with the budget the recovered scenes are bound to.
    Recovered {
        id: u64,
        scenes: Deferred<Vec<(SceneId, SceneRecovery, usize)>>,
        global: GlobalBudget,
        slot: Arc<()>,
    },
}

impl PrepareJob {
    pub(super) fn new(
        id: u64,
        scene: SceneId,
        input: Deferred<PrepareInput>,
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
        let cancel = self.cancel;
        let mut result = self.input.map(|input| {
            layout::prepare(
                input.with_paint_cap(Some(PaintWork::LIMITS)),
                measure,
                &cancel,
            )
        });
        let full_after = result
            .as_ref()
            .map_or(0, |prepared| prepared.after.resident_bytes());
        let owner_bytes = result.as_ref().map_or(0, |prepared| {
            full_after
                .saturating_add(prepared.resident_bytes)
                .saturating_add(prepared.original_bytes())
        });
        result.attribute(owner_bytes);
        Completion::Prepared {
            id: self.id,
            scene: self.scene,
            result,
            full_after,
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

impl AdmitJob {
    pub(super) fn new(
        id: u64,
        read: SceneRead,
        frame: Deferred<DataToShell>,
        cancel: Arc<AtomicBool>,
        slot: Arc<()>,
    ) -> Self {
        Self {
            id,
            read,
            frame,
            cancel,
            slot,
        }
    }

    /// Admits on the calling worker thread, against the snapshot alone.
    pub(crate) fn run(self) -> Completion {
        let result = match &*self.frame {
            DataToShell::SceneStepCommit { step, .. } if !self.cancel.load(Ordering::Relaxed) => {
                Some(self.read.admit_detached(step))
            }
            _ => None,
        };
        Completion::Admitted {
            id: self.id,
            read: self.read,
            result,
            slot: self.slot,
        }
    }

    #[cfg(test)]
    pub(super) fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl RecoveryJob {
    pub(super) fn new(
        id: u64,
        sources: Deferred<Vec<Source>>,
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
        let mut global = GlobalBudget::default();
        let mut scenes = self.sources.map(|sources| {
            let batch: Vec<_> = sources
                .iter()
                .map(|source| (&source.header.begin, source.log.as_slice()))
                .collect();
            let (results, budget) = layout::recover_all(
                &batch,
                self.surface,
                &self.theme,
                measure,
                &self.cancel,
                Some(PaintWork::LIMITS),
            );
            global = budget;
            sources
                .iter()
                .map(|source| source.scene)
                .zip(results)
                .map(|(scene, result)| {
                    let retained = result.as_ref().map_or(0, |(_, rest)| rest.resident_bytes());
                    (scene, result, retained)
                })
                .collect::<Vec<_>>()
        });
        scenes.attribute(scenes.iter().fold(0_usize, |bytes, (_, result, retained)| {
            bytes.saturating_add(result.as_ref().map_or(0, |(state, _)| {
                state.attributed_bytes().saturating_add(*retained)
            }))
        }));
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

impl fmt::Debug for AdmitJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmitJob")
            .field("id", &self.id)
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
            Self::Admitted { id, result, .. } => f
                .debug_struct("Admitted")
                .field("id", id)
                .field("ok", &result.as_ref().map(Result::is_ok))
                .finish_non_exhaustive(),
            Self::Recovered { id, scenes, .. } => f
                .debug_struct("Recovered")
                .field("id", id)
                .field("scenes", &scenes.len())
                .finish_non_exhaustive(),
        }
    }
}
