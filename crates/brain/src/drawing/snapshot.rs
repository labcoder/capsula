//! A connection's recovery: the visible scenes' descriptors and their complete logs in chunks,
//! each checked by the same rules the shell applies, whole frames included, so a brain bug can't
//! make the shell drop the connection on every reconnect.

use std::fmt;

use kumochi_protocol::frame::{self, Pipe};
use kumochi_protocol::message::{
    DataToShell, MAX_VISIBLE_SCENES, SceneDataError, SceneDescriptorV1, SceneHeader, SceneId,
    SceneSnapshotChunk,
};
use kumochi_scene::SceneEvent;
use kumochi_scene::text::Bounded;

use super::{Drawing, Held};

/// What a chunk packs up to (guess). A target, not a maximum: a bigger event goes alone.
pub(crate) const CHUNK_TARGET: usize = 32 * 1024;

/// The visible scenes for a new connection, copied so later changes can't alter it. Its chunks
/// carry drawing content, so `Debug` prints only counts.
pub struct Snapshot {
    /// Membership, oldest first.
    pub scenes: Bounded<SceneDescriptorV1, MAX_VISIBLE_SCENES>,
    /// Every scene's whole log in order, its header in chunk 0, `last` on its final chunk.
    pub chunks: Vec<SceneSnapshotChunk>,
    /// Scenes left out because the shell would refuse what the brain built; clear them durably.
    pub skipped: Vec<SceneId>,
}

impl Drawing {
    /// The descriptors and chunks for a new connection. A scene the shell would refuse, or one past
    /// the fourth, which only a brain bug produces, is left out whole and named in `skipped`.
    /// # Panics
    /// Only if the descriptor collection violates its enforced four-scene limit.
    pub fn snapshot(&self) -> Snapshot {
        let mut scenes = Vec::new();
        let mut chunks = Vec::new();
        let mut skipped = Vec::new();
        for held in self.scenes.iter().filter(|held| !held.gone) {
            if scenes.len() == MAX_VISIBLE_SCENES {
                skipped.push(held.scene);
                continue;
            }
            match described(held) {
                Ok((descriptor, scene_chunks)) => {
                    scenes.push(descriptor);
                    chunks.extend(scene_chunks);
                }
                Err(_) => skipped.push(held.scene),
            }
        }
        let scenes = Bounded::new(scenes).expect("at most four descriptors are kept");
        Snapshot {
            scenes,
            chunks,
            skipped,
        }
    }
}

/// One scene's descriptor and chunks, checked as the shell's decoder and assembler check them.
fn described(held: &Held) -> Result<(SceneDescriptorV1, Vec<SceneSnapshotChunk>), SceneDataError> {
    let state = &held.state;
    let log = state.recovery_log();
    let revision = state.revision();
    let descriptor = SceneDescriptorV1::new(
        held.scene,
        revision,
        u32::try_from(state.committed()).map_err(|_| SceneDataError::Descriptor)?,
        u16::try_from(log.len()).map_err(|_| SceneDataError::Descriptor)?,
    )?;
    let mut last = 0;
    for event in log {
        match event {
            SceneEvent::Step { revision, .. } if *revision == last + 1 => last = *revision,
            SceneEvent::Stop(cut) if last > 0 && cut.activated <= last => {}
            SceneEvent::Step { .. } | SceneEvent::Stop(_) => return Err(SceneDataError::Chunk),
        }
    }
    if last != revision {
        return Err(SceneDataError::Chunk);
    }
    let mut header = Some(SceneHeader {
        begin: state.header().clone(),
        layout_version: held.layout_version,
    });
    let mut chunks = Vec::new();
    let (mut start, mut ordinal) = (0, 0);
    while start < log.len() {
        let end = pack(log, start);
        let chunk = SceneSnapshotChunk {
            scene: held.scene,
            revision,
            ordinal,
            header: header.take(),
            events: Bounded::new(log[start..end].to_vec()).map_err(|_| SceneDataError::Chunk)?,
            last: end == log.len(),
        }
        .checked()?;
        frame::encode(
            &DataToShell::SceneSnapshotV1 {
                chunk: chunk.clone(),
            },
            Pipe::Data,
        )
        .map_err(|_| SceneDataError::Chunk)?;
        chunks.push(chunk);
        (start, ordinal) = (end, ordinal + 1);
    }
    Ok((descriptor, chunks))
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("scenes", &self.scenes.len())
            .field("chunks", &self.chunks.len())
            .field("skipped", &self.skipped.len())
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
