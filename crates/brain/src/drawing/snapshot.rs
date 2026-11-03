//! A connection's recovery: the visible scenes' descriptors and their complete logs in chunks,
//! each checked by the same rules the shell's decoder applies, so a brain bug can't make the
//! shell drop the connection on every reconnect.

use std::fmt;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    MAX_VISIBLE_SCENES, SceneDataError, SceneDescriptorV1, SceneHeader, SceneSnapshotChunk,
};
use kumochi_scene::SceneEvent;
use kumochi_scene::text::Bounded;

use super::Drawing;

/// What a chunk packs up to (guess). A target, not a maximum: a bigger event goes alone.
const CHUNK_TARGET: usize = 32 * 1024;

/// The visible scenes for a new connection, copied so later changes can't alter it. Its chunks
/// carry drawing content, so `Debug` prints only counts.
pub struct Snapshot {
    /// Membership, oldest first.
    pub scenes: Bounded<SceneDescriptorV1, MAX_VISIBLE_SCENES>,
    /// Every scene's whole log in order, its header in chunk 0, `last` on its final chunk.
    pub chunks: Vec<SceneSnapshotChunk>,
}

impl Drawing {
    /// The descriptors and chunks for a new connection.
    ///
    /// # Errors
    /// A descriptor or chunk the shell would refuse, which only a brain bug produces; send no
    /// v1 recovery then.
    pub fn snapshot(&self) -> Result<Snapshot, SceneDataError> {
        let mut scenes = Vec::new();
        let mut chunks = Vec::new();
        for held in self.scenes.iter().filter(|held| !held.gone) {
            let state = &held.state;
            let log = state.recovery_log();
            let revision = state.revision();
            scenes.push(SceneDescriptorV1::new(
                held.scene,
                revision,
                u32::try_from(state.committed()).map_err(|_| SceneDataError::Descriptor)?,
                u16::try_from(log.len()).map_err(|_| SceneDataError::Descriptor)?,
            )?);
            let mut header = Some(SceneHeader {
                begin: state.header().clone(),
                layout_version: held.layout_version,
            });
            let (mut start, mut ordinal) = (0, 0);
            while start < log.len() {
                let end = pack(log, start);
                let events =
                    Bounded::new(log[start..end].to_vec()).map_err(|_| SceneDataError::Chunk)?;
                chunks.push(
                    SceneSnapshotChunk {
                        scene: held.scene,
                        revision,
                        ordinal,
                        header: header.take(),
                        events,
                        last: end == log.len(),
                    }
                    .checked()?,
                );
                (start, ordinal) = (end, ordinal + 1);
            }
        }
        let scenes = Bounded::new(scenes).map_err(|_| SceneDataError::Descriptor)?;
        Ok(Snapshot { scenes, chunks })
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("scenes", &self.scenes.len())
            .field("chunks", &self.chunks.len())
            .finish()
    }
}

/// The end of the chunk that starts at `start`: whole events up to the target, at least one.
fn pack(log: &[SceneEvent], start: usize) -> usize {
    let mut bytes = 0_usize;
    let mut end = start;
    while let Some(event) = log.get(end) {
        // An event too big to encode alone goes alone, and its frame's encode reports it.
        let size = frame::encode(event, Pipe::Data)
            .as_ref()
            .map_or(usize::MAX, Vec::len);
        bytes = bytes.saturating_add(size);
        if end > start && bytes > CHUNK_TARGET {
            break;
        }
        end += 1;
    }
    end
}
