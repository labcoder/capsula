//! The pulled Kumo as the dock draws it: the body, baked once at the carry width and stretched
//! about its bottom centre, the goo shaded each frame, and the device pixels they can show,
//! which the hit mask owns while the dock shows them. How it fits: docs/development/kumo.md,
//! "The pulled picture".

use std::sync::Arc;

use gpui_kit::RenderImage;

use super::kumo::{
    CARRY_WIDTH, FrameState, GooMismatch, GooShader, Look, MAX_SCALE, Piece, Pull, Quad, Rect,
    Renderer, SNAP_LEN, Silhouette, SizeMismatch, Span, device_spans, row_hulls, v2,
};
use super::pulling::PullSpace;
use crate::platform::{ROW_SPANS, RowSpans};

/// The largest pull area for a dock `slot` at `scale`, physical pixels: the slot grown on every
/// side by the snap length and the carry width, with a pixel to spare for rounding. The hit
/// mask is allocated this size once.
pub(super) fn largest_area(slot: [i32; 2], scale: f32) -> [i32; 2] {
    let reach = whole(f64::from((SNAP_LEN + CARRY_WIDTH) * scale).ceil()) + 1;
    slot.map(|side| side + 2 * reach)
}

/// A mask row would need more than [`ROW_SPANS`] spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("mask row {0} needs more than {ROW_SPANS} spans")]
pub(super) struct RowFull(i32);

/// Owned columns for each screen row from `top`, at most [`ROW_SPANS`] a row, left to right.
/// Spans that touch or overlap merge; one more than a row holds is refused, never truncated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Rows {
    top: i32,
    rows: Vec<RowSpans>,
}

impl Rows {
    /// Empties it for `rows` rows from `top`, keeping what it has allocated.
    pub(super) fn clear(&mut self, top: i32, rows: usize) {
        self.top = top;
        self.rows.clear();
        self.rows.resize(rows, [None; ROW_SPANS]);
    }

    pub(super) fn row(&self, y: i32) -> RowSpans {
        self.index(y)
            .map_or([None; ROW_SPANS], |index| self.rows[index])
    }

    /// Adds `[left, right)` to row `y`. Outside the rows it's dropped: nothing draws there.
    pub(super) fn push(&mut self, y: i32, [left, right]: [i32; 2]) -> Result<(), RowFull> {
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
    pub(super) fn extend(&mut self, other: &Self) -> Result<(), RowFull> {
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
pub(super) enum PictureError {
    #[error(transparent)]
    Body(#[from] SizeMismatch),
    #[error(transparent)]
    Goo(#[from] GooMismatch),
    #[error(transparent)]
    Mask(#[from] RowFull),
    #[error("a pulled image has no pixels")]
    Empty,
}

/// One pulled frame: the pull area, and each image with where it's drawn, physical screen
/// pixels, the goo first so the body draws over it.
#[derive(Clone)]
pub(super) struct PullPicture {
    /// The overlay and the mask cover it.
    pub(super) area: [i32; 4],
    pub(super) images: [Option<(Quad, Arc<RenderImage>)>; 3],
}

/// What a pull draws that's made once, from the carry-width bake: the body's image and its
/// coverage, and the goo's scratch.
pub(super) struct PullArt {
    body: Arc<RenderImage>,
    side: u16,
    hulls: Vec<Span<u16>>,
    neck: GooShader,
    stub: GooShader,
    shape: Silhouette,
    look: Look,
    /// One piece's row hulls and device spans, grown to the largest piece and then reused.
    piece_hulls: Vec<Span<u16>>,
    spans: Vec<Span<i32>>,
}

impl PullArt {
    /// The art for `renderer`'s bake, `side` device pixels square, of `shape` with `look`.
    pub(super) fn new(
        renderer: &Renderer,
        side: u16,
        shape: Silhouette,
        look: Look,
    ) -> Result<Self, PictureError> {
        let mut pixels = vec![0; renderer.image_len()];
        renderer.render(&FrameState::IDLE, &mut pixels)?;
        let mut hulls = vec![None; usize::from(side)];
        row_hulls(&pixels, side, &mut hulls);
        Ok(Self {
            body: wrap(side.into(), side.into(), pixels)?,
            side,
            hulls,
            neck: GooShader::neck(CARRY_WIDTH),
            stub: GooShader::stub(CARRY_WIDTH),
            shape,
            look,
            piece_hulls: Vec::new(),
            spans: Vec::new(),
        })
    }

    /// The picture of `pull` grabbed in `space`, and in `rows` the device pixels it can show.
    pub(super) fn frame(
        &mut self,
        pull: &Pull,
        space: PullSpace,
        rows: &mut Rows,
    ) -> Result<PullPicture, PictureError> {
        let bounds = pull.bounds();
        let [left, top] = space
            .screen(bounds.origin)
            .map(|value| whole(value.floor()));
        let [right, bottom] = space
            .screen(bounds.origin + bounds.size)
            .map(|value| whole(value.ceil()));
        let area = [left, top, right - left, bottom - top];
        rows.clear(top, usize::try_from(area[3]).unwrap_or(0));
        let mut images = [None, None, None];
        let neck = pull.neck();
        let scale = (space.scale * 0.5).min(MAX_SCALE);
        let pieces = [Piece::Neck, Piece::Drip, Piece::Stub]
            .into_iter()
            .filter_map(|piece| Some((piece, neck.bounds(piece)?)));
        for ((piece, region), slot) in pieces.zip(&mut images) {
            let shader = if piece == Piece::Stub {
                &mut self.stub
            } else {
                &mut self.neck
            };
            let (width, height) = GooShader::size(region, scale);
            let mut pixels = vec![0; width * height * 4];
            shader.render(
                neck,
                piece,
                region,
                scale,
                &self.shape,
                &self.look,
                &FrameState::IDLE,
                &mut pixels,
            )?;
            let columns = u16::try_from(width).map_err(|_| PictureError::Empty)?;
            self.piece_hulls.resize(height, None);
            row_hulls(&pixels, columns, &mut self.piece_hulls[..height]);
            let quad = quad(space, region);
            own(
                &self.piece_hulls[..height],
                columns,
                quad,
                &mut self.spans,
                rows,
            )?;
            *slot = Some((quad, wrap(width, height, pixels)?));
        }
        let width = pull.width();
        let stretch = neck.body_scale();
        let size = v2(width * stretch.x, width * stretch.y);
        let body = pull.body();
        let quad = quad(
            space,
            Rect {
                origin: v2(body.x - size.x * 0.5, body.y + width * 0.5 - size.y),
                size,
            },
        );
        own(&self.hulls, self.side, quad, &mut self.spans, rows)?;
        images[2] = Some((quad, Arc::clone(&self.body)));
        Ok(PullPicture { area, images })
    }
}

/// Adds the device pixels an image with row `hulls`, `columns` texels wide, can show at `quad`.
fn own(
    hulls: &[Span<u16>],
    columns: u16,
    quad: Quad,
    spans: &mut Vec<Span<i32>>,
    rows: &mut Rows,
) -> Result<(), RowFull> {
    spans.resize(quad.rows(), None);
    device_spans(hulls, columns, quad, spans);
    for (y, span) in (quad.y - 1..).zip(spans.iter()) {
        if let Some(span) = span {
            rows.push(y, *span)?;
        }
    }
    Ok(())
}

/// Where GPUI draws the pull-space rectangle `r`: each edge snapped as GPUI snaps it.
fn quad(space: PullSpace, r: Rect) -> Quad {
    let snap = |value: f64| whole((value - 0.5).ceil());
    let [left, top] = space.screen(r.origin).map(snap);
    let [right, bottom] = space.screen(r.origin + r.size).map(snap);
    Quad {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

/// Straight-alpha BGRA pixels as an image GPUI draws.
fn wrap(width: usize, height: usize, pixels: Vec<u8>) -> Result<Arc<RenderImage>, PictureError> {
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        return Err(PictureError::Empty);
    };
    let buffer = image::RgbaImage::from_raw(width, height, pixels).ok_or(PictureError::Empty)?;
    Ok(Arc::new(RenderImage::new([image::Frame::new(buffer)])))
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "whole screen coordinates fit i32"
)]
fn whole(value: f64) -> i32 {
    value as i32
}

#[cfg(test)]
mod tests {
    use super::{RowFull, Rows, largest_area};
    use crate::platform::ROW_SPANS;

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
