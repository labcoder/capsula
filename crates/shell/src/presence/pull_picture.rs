//! The pulled Kumo as the dock draws it: the body, baked once at the carry width and stretched
//! about its bottom centre, the goo shaded each frame, and the device pixels they can show,
//! which the hit mask owns while the dock shows them. How it fits: docs/development/kumo.md,
//! "The pulled picture".

use std::sync::Arc;

use gpui_kit::RenderImage;

use super::kumo::{
    CARRY_WIDTH, FrameState, GooShader, Look, MAX_SCALE, Piece, Pull, Quad, Rect, Renderer,
    Silhouette, Span, device_spans, row_hulls, v2,
};
use super::pulling::PullSpace;

pub(super) mod machine;

use machine::whole;
pub(super) use machine::{PictureError, RowFull, Rows, largest_area};

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
    /// The art for `renderer`'s bake, `side` device pixels square, of `shape` with `look`,
    /// with its scratch sized for a pull area `rows` tall, so no frame grows it.
    pub(super) fn new(
        renderer: &Renderer,
        side: u16,
        shape: Silhouette,
        look: Look,
        rows: usize,
    ) -> Result<Self, PictureError> {
        let mut pixels = vec![0; renderer.image_len()];
        renderer.render(&FrameState::IDLE, &mut pixels)?;
        let mut hulls = vec![None; usize::from(side)];
        row_hulls(&pixels, side, &mut hulls);
        let (neck, stub) = (GooShader::neck(CARRY_WIDTH), GooShader::stub(CARRY_WIDTH));
        Ok(Self {
            body: wrap(side.into(), side.into(), pixels)?,
            side,
            hulls,
            piece_hulls: Vec::with_capacity(neck.capacity().max(stub.capacity())),
            spans: Vec::with_capacity(rows + 2),
            neck,
            stub,
            shape,
            look,
        })
    }

    #[cfg(test)]
    pub(super) fn scratch_capacity(&self) -> [usize; 2] {
        [self.piece_hulls.capacity(), self.spans.capacity()]
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
