//! Logical expanded-work charges, separate from prepared display residency.
use crate::{
    Budget, ErrorCode,
    limits::{
        MAX_GLOBAL_CHART_MARKS, MAX_GLOBAL_LIVE_OBJECTS, MAX_GLOBAL_PATH_SEGMENTS,
        MAX_GLOBAL_TABLE_CELLS, MAX_VISIBLE_SCENES,
    },
};
use std::sync::Arc;

/// A snapshot of logical charges, including pending removals and replacements.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Live object identities, including those awaiting removal.
    pub objects: usize,
    /// Expanded cells, including padded cells.
    pub cells: usize,
    /// Expanded chart marks.
    pub marks: usize,
    /// Flattened path segments.
    pub segments: usize,
    /// Motion property owners, including reserved generated motion.
    pub tracks: usize,
    /// Key storage reserved for those owners.
    pub keys: usize,
    /// Committed, uncleared scene lifetimes.
    pub scenes: usize,
}
impl Usage {
    pub(crate) fn add(self, rhs: Self) -> Self {
        Self {
            objects: self.objects + rhs.objects,
            cells: self.cells + rhs.cells,
            marks: self.marks + rhs.marks,
            segments: self.segments + rhs.segments,
            tracks: self.tracks + rhs.tracks,
            keys: self.keys + rhs.keys,
            scenes: self.scenes + rhs.scenes,
        }
    }
    pub(crate) fn peak(self, rhs: Self) -> Self {
        Self {
            objects: self.objects.max(rhs.objects),
            cells: self.cells.max(rhs.cells),
            marks: self.marks.max(rhs.marks),
            segments: self.segments.max(rhs.segments),
            tracks: self.tracks.max(rhs.tracks),
            keys: self.keys.max(rhs.keys),
            scenes: self.scenes.max(rhs.scenes),
        }
    }
    fn subtract(self, rhs: Self) -> Self {
        Self {
            objects: self
                .objects
                .checked_sub(rhs.objects)
                .expect("scene objects were charged"),
            cells: self
                .cells
                .checked_sub(rhs.cells)
                .expect("scene cells were charged"),
            marks: self
                .marks
                .checked_sub(rhs.marks)
                .expect("scene marks were charged"),
            segments: self
                .segments
                .checked_sub(rhs.segments)
                .expect("scene segments were charged"),
            tracks: self
                .tracks
                .checked_sub(rhs.tracks)
                .expect("scene tracks were charged"),
            keys: self
                .keys
                .checked_sub(rhs.keys)
                .expect("scene keys were charged"),
            scenes: self
                .scenes
                .checked_sub(rhs.scenes)
                .expect("scene lifetimes were charged"),
        }
    }
    pub(crate) fn validate_global(self) -> Result<(), ErrorCode> {
        for (actual, cap, budget) in [
            (self.objects, MAX_GLOBAL_LIVE_OBJECTS, Budget::LiveObjects),
            (self.cells, MAX_GLOBAL_TABLE_CELLS, Budget::TableCells),
            (self.marks, MAX_GLOBAL_CHART_MARKS, Budget::ChartMarks),
            (
                self.segments,
                MAX_GLOBAL_PATH_SEGMENTS,
                Budget::PathSegments,
            ),
            (self.scenes, MAX_VISIBLE_SCENES, Budget::VisibleScenes),
        ] {
            if actual > cap {
                return Err(ErrorCode::OverBudget(budget));
            }
        }
        Ok(())
    }
}

/// Shared admission context for visible scenes in one process.
///
/// An admission is valid only for this instance and its current generation.
#[derive(Debug)]
pub struct GlobalBudget {
    pub(crate) identity: Arc<()>,
    generation: u64,
    usage: Usage,
    pub(crate) replaying: bool,
}
impl Default for GlobalBudget {
    fn default() -> Self {
        Self {
            identity: Arc::new(()),
            generation: 0,
            usage: Usage::default(),
            replaying: false,
        }
    }
}
impl GlobalBudget {
    /// Returns the local generation used to reject stale admissions.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Returns the currently charged logical work.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    pub(crate) fn replay() -> Self {
        Self {
            replaying: true,
            ..Self::default()
        }
    }
    pub(crate) fn check(&self, old: Usage, new: Usage) -> Result<(), ErrorCode> {
        if self.replaying {
            return Ok(());
        }
        self.usage.subtract(old).add(new).validate_global()
    }
    pub(crate) fn replace(&mut self, old: Usage, new: Usage) {
        self.usage = self.usage.subtract(old).add(new);
        self.advance();
    }
    pub(crate) fn install(&mut self, usage: Usage) {
        self.usage = usage;
        self.advance();
    }
    pub(crate) fn advance(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("budget changes cannot exhaust u64 within a process lifetime");
    }
}
