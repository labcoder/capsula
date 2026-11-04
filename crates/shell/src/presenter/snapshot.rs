//! Bounded wire-log assembly; semantic replay and presentation belong to the caller.
//!
//! Chunks, logs and batches drop on the dropper thread, whichever path discards them.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
};

use kumochi_protocol::{
    frame::{HEADER_LEN, Pipe},
    message::{
        DATA_WINDOW, MAX_SCENE_LOG_ENTRIES, MAX_SCENE_SNAPSHOT_BYTES, MAX_SCENE_STAGING_BYTES,
        MAX_VISIBLE_SCENES, SceneDescriptorV1, SceneHeader, SceneId, SceneSnapshotChunk,
    },
};
use kumochi_scene::{SceneEvent, deferred::Deferred, limits, text::Bounded};

use super::wire_bytes;

const CLEARED_KEPT: usize = 64;
const MAX_FRAME_BYTES: usize = Pipe::Data.max_payload() + HEADER_LEN;
const _: () = assert!(
    MAX_SCENE_SNAPSHOT_BYTES >= MAX_SCENE_LOG_ENTRIES * Pipe::Data.max_payload(),
    "the scene budget holds a full log of maximum payloads"
);
const _: () = assert!(
    MAX_SCENE_SNAPSHOT_BYTES < MAX_SCENE_LOG_ENTRIES * MAX_FRAME_BYTES,
    "frame headers also consume the scene budget"
);
const _: () = assert!(
    MAX_SCENE_STAGING_BYTES == MAX_VISIBLE_SCENES * MAX_SCENE_SNAPSHOT_BYTES,
    "the global budget holds four scene budgets"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Broken {
    #[error("invalid snapshot membership")]
    Membership,
    #[error("invalid snapshot counts")]
    Counts,
    #[error("invalid snapshot sequence")]
    Sequence,
    #[error("invalid snapshot frame size")]
    Frame,
    #[error("snapshot staging budget exceeded")]
    Bytes,
    #[error("snapshot credit window exceeded")]
    Window,
}

#[derive(Clone, PartialEq)]
pub(crate) struct Source {
    pub(crate) scene: SceneId,
    pub(crate) header: SceneHeader,
    pub(crate) revision: u32,
    pub(crate) committed: u32,
    pub(crate) log: Vec<SceneEvent>,
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Source")
            .field("scene", &self.scene)
            .field("revision", &self.revision)
            .field("committed", &self.committed)
            .field("entries", &self.log.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub(crate) struct Effect {
    pub(crate) credit: u32,
    pub(crate) clear: Vec<SceneId>,
    pub(crate) batch: Option<Deferred<Vec<Source>>>,
}

#[cfg(test)]
impl PartialEq for Effect {
    fn eq(&self, other: &Self) -> bool {
        self.credit == other.credit
            && self.clear == other.clear
            && self.batch.as_deref() == other.batch.as_deref()
    }
}

struct Pending {
    descriptor: SceneDescriptorV1,
    header: Option<SceneHeader>,
    log: Vec<SceneEvent>,
    ordinal: u16,
    revision: u32,
    kept: [bool; limits::MAX_STEPS],
    bytes: usize,
    done: bool,
}

#[derive(Default)]
pub(crate) struct Assembler {
    generation: Option<u64>,
    described: bool,
    held: VecDeque<(Deferred<SceneSnapshotChunk>, u32)>,
    held_bytes: u32,
    scenes: BTreeMap<SceneId, Pending>,
    bytes: usize,
    delivered: bool,
    clear_intent: bool,
    cleared: VecDeque<SceneId>,
}

impl fmt::Debug for Assembler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Assembler")
            .field("generation", &self.generation)
            .field("described", &self.described)
            .field("held_bytes", &self.held_bytes)
            .field("scenes", &self.scenes.len())
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl Assembler {
    pub(crate) fn connected(&mut self, generation: u64) {
        self.disconnected();
        self.generation = Some(generation);
    }

    pub(crate) fn disconnected(&mut self) {
        self.generation = None;
        self.described = false;
        self.retire_held();
        drop(Deferred::new(std::mem::take(&mut self.scenes), self.bytes));
        self.bytes = 0;
        self.delivered = false;
    }

    pub(crate) fn described(&self) -> bool {
        self.described
    }

    pub(crate) fn pending(&self) -> bool {
        self.generation.is_some()
            && if self.described {
                !self.delivered && !self.scenes.is_empty()
            } else {
                !self.clear_intent
            }
    }

    pub(crate) fn describe(
        &mut self,
        generation: u64,
        descriptors: &Bounded<SceneDescriptorV1, MAX_VISIBLE_SCENES>,
    ) -> Result<Effect, Broken> {
        if self.generation != Some(generation) {
            return Ok(Effect::default());
        }
        if self.described {
            return Err(Broken::Membership);
        }
        for (at, descriptor) in descriptors.iter().enumerate() {
            SceneDescriptorV1::new(
                descriptor.scene,
                descriptor.revision,
                descriptor.committed,
                descriptor.log_entries,
            )
            .map_err(|_| Broken::Counts)?;
            if descriptors[..at]
                .iter()
                .any(|earlier| earlier.scene == descriptor.scene)
            {
                return Err(Broken::Membership);
            }
        }
        let mut effect = Effect::default();
        for descriptor in descriptors.iter().copied() {
            if self.clear_intent || self.cleared.contains(&descriptor.scene) {
                effect.clear.push(descriptor.scene);
                self.remember(descriptor.scene);
            } else {
                self.scenes.insert(
                    descriptor.scene,
                    Pending {
                        descriptor,
                        header: None,
                        log: Vec::new(),
                        ordinal: 0,
                        revision: 0,
                        kept: [false; limits::MAX_STEPS],
                        bytes: 0,
                        done: false,
                    },
                );
            }
        }
        self.clear_intent = false;
        self.described = true;
        let mut held = Deferred::new(std::mem::take(&mut self.held), wire_bytes(self.held_bytes));
        self.held_bytes = 0;
        while let Some((chunk, bytes)) = held.pop_front() {
            self.accept(chunk, bytes)?;
            effect.credit = effect.credit.checked_add(bytes).ok_or(Broken::Window)?;
        }
        effect.batch = self.finish();
        Ok(effect)
    }

    /// `bytes` is the received payload plus its four-byte frame header.
    pub(crate) fn chunk(
        &mut self,
        generation: u64,
        chunk: Deferred<SceneSnapshotChunk>,
        bytes: u32,
    ) -> Result<Effect, Broken> {
        if self.generation != Some(generation) {
            return Ok(Effect::default());
        }
        let size = usize::try_from(bytes).map_err(|_| Broken::Frame)?;
        if !(HEADER_LEN + 1..=MAX_FRAME_BYTES).contains(&size) {
            return Err(Broken::Frame);
        }
        chunk.validate().map_err(|_| Broken::Sequence)?;
        if !self.described && !self.clear_intent && !self.cleared.contains(&chunk.scene) {
            let held_bytes = self.held_bytes.checked_add(bytes).ok_or(Broken::Window)?;
            if held_bytes > DATA_WINDOW {
                return Err(Broken::Window);
            }
            self.held.push_back((chunk, bytes));
            self.held_bytes = held_bytes;
            return Ok(Effect::default());
        }
        if self.described {
            self.accept(chunk, bytes)?;
        }
        Ok(Effect {
            credit: bytes,
            batch: self.finish(),
            ..Effect::default()
        })
    }

    fn accept(
        &mut self,
        mut chunk: Deferred<SceneSnapshotChunk>,
        bytes: u32,
    ) -> Result<(), Broken> {
        if self.cleared.contains(&chunk.scene) {
            return Ok(());
        }
        let pending = self
            .scenes
            .get_mut(&chunk.scene)
            .ok_or(Broken::Membership)?;
        if pending.done
            || chunk.ordinal != pending.ordinal
            || chunk.revision != pending.descriptor.revision
        {
            return Err(Broken::Sequence);
        }
        let count = pending
            .log
            .len()
            .checked_add(chunk.events.len())
            .ok_or(Broken::Counts)?;
        if count > usize::from(pending.descriptor.log_entries)
            || chunk.last != (count == usize::from(pending.descriptor.log_entries))
        {
            return Err(Broken::Counts);
        }
        let size = usize::try_from(bytes).map_err(|_| Broken::Frame)?;
        let scene_bytes = pending.bytes.checked_add(size).ok_or(Broken::Bytes)?;
        let total_bytes = self.bytes.checked_add(size).ok_or(Broken::Bytes)?;
        if scene_bytes > MAX_SCENE_SNAPSHOT_BYTES || total_bytes > MAX_SCENE_STAGING_BYTES {
            return Err(Broken::Bytes);
        }
        let mut revision = pending.revision;
        let mut kept = pending.kept;
        for event in chunk.events.iter() {
            match event {
                SceneEvent::Step { revision: next, .. } => {
                    if *next != revision + 1 || *next > pending.descriptor.revision {
                        return Err(Broken::Sequence);
                    }
                    revision = *next;
                    let index = usize::try_from(revision - 1).map_err(|_| Broken::Counts)?;
                    *kept.get_mut(index).ok_or(Broken::Counts)? = true;
                }
                SceneEvent::Stop(cut) => {
                    if revision == 0 || cut.activated > revision {
                        return Err(Broken::Sequence);
                    }
                    let activated = usize::try_from(cut.activated).map_err(|_| Broken::Counts)?;
                    kept[activated..].fill(false);
                }
            }
        }
        if chunk.last
            && (revision != pending.descriptor.revision
                || kept.iter().filter(|kept| **kept).count()
                    != usize::try_from(pending.descriptor.committed).map_err(|_| Broken::Counts)?)
        {
            return Err(Broken::Counts);
        }
        pending.revision = revision;
        pending.kept = kept;
        pending.bytes = scene_bytes;
        self.bytes = total_bytes;
        pending.ordinal += 1;
        pending.done = chunk.last;
        if let Some(header) = chunk.header.take() {
            pending.header = Some(header);
        }
        pending.log.extend(chunk.events.iter().cloned());
        Ok(())
    }

    fn finish(&mut self) -> Option<Deferred<Vec<Source>>> {
        if !self.described || self.delivered || self.scenes.values().any(|pending| !pending.done) {
            return None;
        }
        self.delivered = true;
        Some(Deferred::new(
            self.scenes
                .iter_mut()
                .map(|(scene, pending)| Source {
                    scene: *scene,
                    header: pending
                        .header
                        .take()
                        .expect("a completed snapshot began with a header"),
                    revision: pending.descriptor.revision,
                    committed: pending.descriptor.committed,
                    log: std::mem::take(&mut pending.log),
                })
                .collect(),
            self.bytes,
        ))
    }

    pub(crate) fn remove(&mut self, scenes: &[SceneId]) -> Effect {
        let mut clear = scenes.to_vec();
        clear.sort_unstable();
        clear.dedup();
        for scene in &clear {
            self.remember(*scene);
            if let Some(pending) = self.scenes.remove(scene) {
                self.bytes -= pending.bytes;
                drop(Deferred::new(pending.log, pending.bytes));
            }
        }
        let mut removed = Vec::new();
        for _ in 0..self.held.len() {
            let chunk = self.held.pop_front().expect("a counted chunk remains");
            if clear.contains(&chunk.0.scene) {
                removed.push(chunk);
            } else {
                self.held.push_back(chunk);
            }
        }
        let credit = removed.iter().map(|(_, bytes)| *bytes).sum();
        let bytes = wire_bytes(credit)
            + removed.capacity() * std::mem::size_of::<(Deferred<SceneSnapshotChunk>, u32)>();
        drop(Deferred::new(removed, bytes));
        self.held_bytes -= credit;
        Effect {
            credit,
            clear,
            batch: self.finish(),
        }
    }

    pub(crate) fn clear(&mut self) -> Effect {
        if !self.described {
            self.clear_intent = true;
        }
        let ids: Vec<_> = self.scenes.keys().copied().collect();
        let mut effect = self.remove(&ids);
        effect.credit += self.held_bytes;
        self.retire_held();
        self.delivered = self.described;
        effect.batch = None;
        effect
    }

    fn remember(&mut self, scene: SceneId) {
        if !self.cleared.contains(&scene) {
            self.cleared.push_back(scene);
        }
        if self.cleared.len() > CLEARED_KEPT {
            self.cleared.pop_front();
        }
    }

    /// Hands every held chunk to the dropper in one send.
    fn retire_held(&mut self) {
        drop(Deferred::new(
            std::mem::take(&mut self.held),
            wire_bytes(self.held_bytes),
        ));
        self.held_bytes = 0;
    }
}

impl Drop for Assembler {
    fn drop(&mut self) {
        self.disconnected();
    }
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
