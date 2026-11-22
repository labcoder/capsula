use crate::presence::kumo::{CARRY_WIDTH, GooMismatch, SNAP_LEN, SizeMismatch};
use crate::presence::line::machine::{ROW_SPANS, RowSpans};

/// The largest pull area for a dock `slot` at `scale`, physical pixels: the slot grown on every
/// side by the snap length and the carry width, with a pixel to spare for rounding. The hit
/// mask is allocated this size once.
pub(in crate::presence) fn largest_area(slot: [i32; 2], scale: f32) -> [i32; 2] {
    let reach = whole(f64::from((SNAP_LEN + CARRY_WIDTH) * scale).ceil()) + 1;
    slot.map(|side| side + 2 * reach)
}

/// A mask row would need more than [`ROW_SPANS`] spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("mask row {0} needs more than {ROW_SPANS} spans")]
pub(in crate::presence) struct RowFull(i32);

/// Owned columns for each screen row from `top`, at most [`ROW_SPANS`] a row, left to right.
/// Spans that touch or overlap merge; one more than a row holds is refused, never truncated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::presence) struct Rows {
    top: i32,
    rows: Vec<RowSpans>,
}

impl Rows {
    /// Room for `rows` rows, so filling them never allocates.
    pub(in crate::presence) fn with_capacity(rows: usize) -> Self {
        Self {
            top: 0,
            rows: Vec::with_capacity(rows),
        }
    }

    #[cfg(test)]
    pub(in crate::presence) fn capacity(&self) -> usize {
        self.rows.capacity()
    }

    /// Empties it for `rows` rows from `top`, keeping what it has allocated.
    pub(in crate::presence) fn clear(&mut self, top: i32, rows: usize) {
        self.top = top;
        self.rows.clear();
        self.rows.resize(rows, [None; ROW_SPANS]);
    }

    pub(in crate::presence) fn row(&self, y: i32) -> RowSpans {
        self.index(y)
            .map_or([None; ROW_SPANS], |index| self.rows[index])
    }

    /// Adds `[left, right)` to row `y`. Outside the rows it's dropped: nothing draws there.
    pub(in crate::presence) fn push(
        &mut self,
        y: i32,
        [left, right]: [i32; 2],
    ) -> Result<(), RowFull> {
        let Some(index) = self.index(y).filter(|_| left < right) else {
            return Ok(());
        };
        let row = &mut self.rows[index];
        let mut all = [None; ROW_SPANS + 1];
        all[..ROW_SPANS].copy_from_slice(row);
        all[ROW_SPANS] = Some([left, right]);
        all.sort_by_key(|span| span.map_or(i32::MAX, |[left, _]| left));
        let mut merged: RowSpans = [None; ROW_SPANS];
        let mut count: usize = 0;
        for [left, right] in all.into_iter().flatten() {
            if let Some(Some([_, end])) = count.checked_sub(1).map(|last| &mut merged[last])
                && left <= *end
            {
                *end = (*end).max(right);
                continue;
            }
            if count == ROW_SPANS {
                return Err(RowFull(y));
            }
            merged[count] = Some([left, right]);
            count += 1;
        }
        *row = merged;
        Ok(())
    }

    /// Adds every span of `other`.
    pub(in crate::presence) fn extend(&mut self, other: &Self) -> Result<(), RowFull> {
        for (y, row) in (other.top..).zip(&other.rows) {
            for span in row.iter().flatten() {
                self.push(y, *span)?;
            }
        }
        Ok(())
    }

    fn index(&self, y: i32) -> Option<usize> {
        usize::try_from(y - self.top)
            .ok()
            .filter(|&index| index < self.rows.len())
    }
}

/// Why a pulled picture couldn't be made; the dock keeps the one it shows.
#[derive(Debug, thiserror::Error)]
pub(in crate::presence) enum PictureError {
    #[error(transparent)]
    Body(#[from] SizeMismatch),
    #[error(transparent)]
    Goo(#[from] GooMismatch),
    #[error(transparent)]
    Mask(#[from] RowFull),
    #[error("a pulled image has no pixels")]
    Empty,
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "whole screen coordinates fit i32"
)]
pub(in crate::presence) fn whole(value: f64) -> i32 {
    value as i32
}

#[cfg(test)]
mod tests {
    use super::{ROW_SPANS, RowFull, Rows, largest_area};

    fn rows() -> Rows {
        let mut rows = Rows::default();
        rows.clear(10, 2);
        rows
    }

    #[test]
    fn a_row_holds_four_disjoint_spans_in_order_and_refuses_a_fifth() {
        let mut rows = rows();
        for span in [[40, 50], [0, 5], [20, 30], [60, 61]] {
            rows.push(10, span).expect("room for four");
        }
        assert_eq!(
            rows.row(10),
            [Some([0, 5]), Some([20, 30]), Some([40, 50]), Some([60, 61])]
        );
        let before = rows.row(10);
        assert_eq!(rows.push(10, [8, 9]), Err(RowFull(10)));
        assert_eq!(rows.row(10), before, "a refused span changes nothing");
        assert_eq!(rows.row(11), [None; ROW_SPANS]);
    }

    #[test]
    fn touching_and_overlapping_spans_merge_even_across_a_gap_they_bridge() {
        let mut rows = rows();
        for span in [[0, 5], [10, 15], [20, 25], [30, 35]] {
            rows.push(11, span).expect("room for four");
        }
        rows.push(11, [5, 20]).expect("it bridges three into one");
        assert_eq!(rows.row(11), [Some([0, 25]), Some([30, 35]), None, None]);
        rows.push(11, [35, 40]).expect("touching at 35");
        assert_eq!(rows.row(11), [Some([0, 25]), Some([30, 40]), None, None]);
    }

    #[test]
    fn spans_outside_the_rows_or_empty_are_dropped() {
        let mut rows = rows();
        rows.push(9, [0, 5]).expect("above");
        rows.push(12, [0, 5]).expect("below");
        rows.push(10, [5, 5]).expect("empty");
        assert_eq!(rows, self::rows());
    }

    #[test]
    fn extending_merges_another_shapes_rows_within_its_own() {
        let mut line = rows();
        line.push(10, [0, 10]).expect("room");
        let mut body = Rows::default();
        body.clear(11, 3);
        body.push(11, [20, 30]).expect("room");
        body.push(12, [5, 25]).expect("outside the line's rows");
        line.extend(&body).expect("room");
        assert_eq!(line.row(10), [Some([0, 10]), None, None, None]);
        assert_eq!(line.row(11), [Some([20, 30]), None, None, None]);
        assert_eq!(line.row(12), [None; ROW_SPANS]);
    }

    #[test]
    fn the_largest_area_grows_the_slot_by_the_reach_on_every_side() {
        assert_eq!(largest_area([58, 21], 1.0), [58 + 458, 21 + 458]);
        assert_eq!(largest_area([86, 30], 1.5), [86 + 686, 30 + 686]);
    }
}
