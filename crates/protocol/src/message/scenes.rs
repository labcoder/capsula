//! Bounded scene v1 wire data; semantic replay validation belongs to the consumer.

use std::{fmt, ops::Deref};

use kumochi_scene::{Begin, SceneEvent, StopCut, limits, text::Bounded};
use serde::{Deserialize, Serialize};

use super::SceneId;

/// Original steps and effective cuts in one scene's lifetime log.
pub const MAX_SCENE_LOG_ENTRIES: usize = 2 * limits::MAX_STEPS;
/// Encoded staging policy per scene, including frame headers, apart from prepared geometry.
pub const MAX_SCENE_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
/// Four encoded snapshots; separate from the 64 MiB prepared-geometry residency budget.
pub const MAX_SCENE_STAGING_BYTES: usize = 4 * MAX_SCENE_SNAPSHOT_BYTES;

const _: () = assert!(
    super::MAX_VISIBLE_SCENES == limits::MAX_VISIBLE_SCENES,
    "protocol and scene must share the visible scene bound"
);

/// The original scene header and the layout algorithm used to prepare it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneHeader {
    /// Original model input; never log its content.
    pub begin: Begin,
    /// Layout version persisted with the scene.
    pub layout_version: u32,
}

impl fmt::Debug for SceneHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SceneHeader")
            .field("layout_version", &self.layout_version)
            .finish_non_exhaustive()
    }
}

/// A visible scene's lifetime revision, retained count and complete log length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "DescriptorWire")]
pub struct SceneDescriptorV1 {
    /// Store-assigned scene identity.
    pub scene: SceneId,
    /// Last committed revision, including steps later cut away.
    pub revision: u32,
    /// Positive number of retained steps, no greater than the revision.
    pub committed: u32,
    /// Original steps plus effective cuts, not just retained steps.
    pub log_entries: u16,
}

#[derive(Deserialize)]
struct DescriptorWire {
    scene: SceneId,
    revision: u32,
    committed: u32,
    log_entries: u16,
}

impl SceneDescriptorV1 {
    /// Checks producer-built counts with the same rules used by the decoder.
    /// # Errors
    /// Returns `Descriptor` for inconsistent lifetime, retained or log counts.
    pub fn new(
        scene: SceneId,
        revision: u32,
        committed: u32,
        log_entries: u16,
    ) -> Result<Self, SceneDataError> {
        DescriptorWire {
            scene,
            revision,
            committed,
            log_entries,
        }
        .try_into()
    }
}

impl TryFrom<DescriptorWire> for SceneDescriptorV1 {
    type Error = SceneDataError;

    fn try_from(value: DescriptorWire) -> Result<Self, Self::Error> {
        if !valid_revision(value.revision)
            || value.committed == 0
            || value.committed > value.revision
            || !(value.revision..=2 * value.revision).contains(&u32::from(value.log_entries))
        {
            return Err(SceneDataError::Descriptor);
        }
        Ok(Self {
            scene: value.scene,
            revision: value.revision,
            committed: value.committed,
            log_entries: value.log_entries,
        })
    }
}

/// A cut is keyed by scene, independently of whichever run last committed there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneCut {
    /// Store-assigned scene identity.
    pub scene: SceneId,
    /// Activated bound and finished object handles.
    pub cut: StopCut,
}

/// At most four distinct scenes and 1,500 finished handles across their cuts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Bounded<SceneCut, { limits::MAX_VISIBLE_SCENES }>")]
pub struct SceneCuts(Bounded<SceneCut, { limits::MAX_VISIBLE_SCENES }>);

impl SceneCuts {
    /// Checks scene identities, handle uniqueness and the aggregate object cap.
    /// # Errors
    /// Returns a content-free error for an excessive or duplicate cut list.
    pub fn new(cuts: Vec<SceneCut>) -> Result<Self, SceneDataError> {
        Bounded::new(cuts)
            .map_err(|_| SceneDataError::Cuts)
            .and_then(Self::try_from)
    }
}

impl TryFrom<Bounded<SceneCut, { limits::MAX_VISIBLE_SCENES }>> for SceneCuts {
    type Error = SceneDataError;

    fn try_from(
        cuts: Bounded<SceneCut, { limits::MAX_VISIBLE_SCENES }>,
    ) -> Result<Self, Self::Error> {
        let mut total = 0;
        for (index, cut) in cuts.iter().enumerate() {
            if cuts[..index]
                .iter()
                .any(|earlier| earlier.scene == cut.scene)
                || cut
                    .cut
                    .revealed
                    .iter()
                    .enumerate()
                    .any(|(at, handle)| cut.cut.revealed[..at].contains(handle))
            {
                return Err(SceneDataError::Cuts);
            }
            total += cut.cut.revealed.len();
        }
        if total > limits::MAX_GLOBAL_LIVE_OBJECTS {
            return Err(SceneDataError::Cuts);
        }
        Ok(Self(cuts))
    }
}

impl Deref for SceneCuts {
    type Target = [SceneCut];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// One nonempty piece of a complete lifetime log, ordered within a scene's snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ChunkWire")]
pub struct SceneSnapshotChunk {
    /// Store-assigned scene identity.
    pub scene: SceneId,
    /// Revision advertised by the descriptor.
    pub revision: u32,
    /// Zero-based chunk number, below the maximum log length.
    pub ordinal: u16,
    /// Present exactly in chunk zero.
    pub header: Option<SceneHeader>,
    /// Complete original steps and effective cuts, never empty.
    pub events: Bounded<SceneEvent, MAX_SCENE_LOG_ENTRIES>,
    /// This completes the snapshot; cross-chunk checks belong to recovery.
    pub last: bool,
}

#[derive(Deserialize)]
struct ChunkWire {
    scene: SceneId,
    revision: u32,
    ordinal: u16,
    header: Option<SceneHeader>,
    events: Bounded<SceneEvent, MAX_SCENE_LOG_ENTRIES>,
    last: bool,
}

impl SceneSnapshotChunk {
    /// Checks producer-built chunk shape with the same rules used by the decoder.
    /// # Errors
    /// Returns `Chunk` for invalid header placement, counts or revisions.
    pub fn checked(self) -> Result<Self, SceneDataError> {
        ChunkWire {
            scene: self.scene,
            revision: self.revision,
            ordinal: self.ordinal,
            header: self.header,
            events: self.events,
            last: self.last,
        }
        .try_into()
    }
}

impl TryFrom<ChunkWire> for SceneSnapshotChunk {
    type Error = SceneDataError;

    fn try_from(value: ChunkWire) -> Result<Self, Self::Error> {
        if !valid_revision(value.revision)
            || usize::from(value.ordinal) >= MAX_SCENE_LOG_ENTRIES
            || value.events.is_empty()
            || value.header.is_some() != (value.ordinal == 0)
            || value.events.iter().any(|event| {
                matches!(event, SceneEvent::Step { revision, .. } if !valid_revision(*revision) || *revision > value.revision)
            })
        {
            return Err(SceneDataError::Chunk);
        }
        Ok(Self {
            scene: value.scene,
            revision: value.revision,
            ordinal: value.ordinal,
            header: value.header,
            events: value.events,
            last: value.last,
        })
    }
}

fn valid_revision(revision: u32) -> bool {
    usize::try_from(revision).is_ok_and(|revision| (1..=limits::MAX_STEPS).contains(&revision))
}

/// Local wire validation failures, never carrying drawing content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SceneDataError {
    /// Inconsistent visible scene counts.
    #[error("invalid scene descriptor")]
    Descriptor,
    /// Duplicate identities or excessive cut counts.
    #[error("invalid scene cuts")]
    Cuts,
    /// Invalid chunk shape or revision.
    #[error("invalid scene snapshot chunk")]
    Chunk,
}

/// Why scene preparation failed. Append only; no content may accompany a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SceneFailure {
    /// Work was cancelled.
    Cancelled,
    /// Preparation exceeded its deadline.
    Timeout,
    /// Semantic validation failed.
    Validation,
    /// Layout failed.
    Layout,
    /// Prepared geometry exceeded its residency budget.
    Residency,
    /// Text measurement failed.
    TextMeasure,
}

#[cfg(test)]
#[path = "scenes_tests.rs"]
mod tests;
