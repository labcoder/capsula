//! Logical expanded-work charges, separate from prepared display residency.
use crate::{
    Budget, ErrorCode,
    limits::{
        MAX_GLOBAL_CHART_MARKS, MAX_GLOBAL_DASH_PIECES, MAX_GLOBAL_LIVE_OBJECTS,
        MAX_GLOBAL_PATH_SEGMENTS, MAX_GLOBAL_TABLE_CELLS, MAX_TRIANGLES, MAX_VISIBLE_SCENES,
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
    /// Dash pieces of dashed strokes whose geometry is explicit; font-dependent ones charge 0.
    pub dash_pieces: usize,
    /// Explicit motion property owners; generated effects are bounded by object count.
    pub tracks: usize,
    /// Key storage reserved for explicit owners.
    pub keys: usize,
    /// Committed, uncleared scene lifetimes.
    pub scenes: usize,
    /// Solids.
    pub solids: usize,
    /// Sized solid triangles, before tessellation.
    pub triangles: usize,
    /// 3D views.
    pub views: usize,
}
impl Usage {
    pub(crate) fn add(self, rhs: Self) -> Self {
        Self {
            objects: self.objects + rhs.objects,
            cells: self.cells + rhs.cells,
            marks: self.marks + rhs.marks,
            segments: self.segments + rhs.segments,
            dash_pieces: self.dash_pieces + rhs.dash_pieces,
            tracks: self.tracks + rhs.tracks,
            keys: self.keys + rhs.keys,
            scenes: self.scenes + rhs.scenes,
            solids: self.solids + rhs.solids,
            triangles: self.triangles + rhs.triangles,
            views: self.views + rhs.views,
        }
    }
    pub(crate) fn peak(self, rhs: Self) -> Self {
        Self {
            objects: self.objects.max(rhs.objects),
            cells: self.cells.max(rhs.cells),
            marks: self.marks.max(rhs.marks),
            segments: self.segments.max(rhs.segments),
            dash_pieces: self.dash_pieces.max(rhs.dash_pieces),
            tracks: self.tracks.max(rhs.tracks),
            keys: self.keys.max(rhs.keys),
            scenes: self.scenes.max(rhs.scenes),
            solids: self.solids.max(rhs.solids),
            triangles: self.triangles.max(rhs.triangles),
            views: self.views.max(rhs.views),
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
            dash_pieces: self
                .dash_pieces
                .checked_sub(rhs.dash_pieces)
                .expect("scene dash pieces were charged"),
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
            solids: self
                .solids
                .checked_sub(rhs.solids)
                .expect("scene solids were charged"),
            triangles: self
                .triangles
                .checked_sub(rhs.triangles)
                .expect("scene triangles were charged"),
            views: self
                .views
                .checked_sub(rhs.views)
                .expect("scene views were charged"),
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
            (self.dash_pieces, MAX_GLOBAL_DASH_PIECES, Budget::DashPieces),
            (self.scenes, MAX_VISIBLE_SCENES, Budget::VisibleScenes),
            (self.triangles, MAX_TRIANGLES, Budget::Triangles),
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
    pub(crate) fn without(&self, removed: Usage) -> Self {
        Self {
            identity: Arc::clone(&self.identity),
            generation: self.generation,
            usage: self.usage.subtract(removed),
            replaying: self.replaying,
        }
    }

    /// Returns the local generation used to reject stale admissions.
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// Returns the currently charged logical work.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Checks cached presentation charges alongside live membership without changing it.
    ///
    /// # Errors
    /// The first global cap exceeded by the combined charges.
    pub fn check_retained(&self, retained: &[Usage]) -> Result<(), ErrorCode> {
        let mut total = self.usage;
        for usage in retained {
            total = Usage {
                objects: total.objects.saturating_add(usage.objects),
                cells: total.cells.saturating_add(usage.cells),
                marks: total.marks.saturating_add(usage.marks),
                segments: total.segments.saturating_add(usage.segments),
                dash_pieces: total.dash_pieces.saturating_add(usage.dash_pieces),
                scenes: total.scenes.saturating_add(usage.scenes),
                triangles: total.triangles.saturating_add(usage.triangles),
                ..total
            };
        }
        total.validate_global()
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

#[cfg(test)]
mod retained_tests {
    use super::*;

    #[test]
    fn retained_objects_share_the_existing_cap_without_changing_membership() {
        let mut global = GlobalBudget::default();
        global.install(Usage {
            objects: 1,
            scenes: 1,
            ..Usage::default()
        });
        let generation = global.generation();
        for (objects, expected) in [
            (MAX_GLOBAL_LIVE_OBJECTS - 1, Ok(())),
            (
                MAX_GLOBAL_LIVE_OBJECTS,
                Err(ErrorCode::OverBudget(Budget::LiveObjects)),
            ),
            (usize::MAX, Err(ErrorCode::OverBudget(Budget::LiveObjects))),
        ] {
            assert_eq!(
                global.check_retained(&[Usage {
                    objects,
                    ..Usage::default()
                }]),
                expected
            );
            assert_eq!(
                global.usage(),
                Usage {
                    objects: 1,
                    scenes: 1,
                    ..Usage::default()
                }
            );
            assert_eq!(global.generation(), generation);
        }
        assert_eq!(global.check_retained(&[]), Ok(()));
    }
}
