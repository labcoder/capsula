//! Which device pixels of a drawn Kumo image take the pointer: a conservative mask. GPUI scales
//! each image with bilinear filtering, so this owns every device pixel whose sample can see a
//! texel at or above the cutoff, plus one pixel for snapping. It never lets a click through a
//! visible pixel, and over-owns only a band about one source texel wide at the edge.

/// The per-layer alpha cutoff. Two layers each under 75/255 composite to under 128/255, so a
/// pixel that is visible, at 128 or more after compositing, has a texel at 75 or more in one of
/// its layers' sampling footprints.
pub(crate) const CUTOFF: u8 = 75;

/// A half-open span of columns, `[left, right)`.
pub(crate) type Span<T> = Option<[T; 2]>;

/// Where GPUI draws a square image: its snapped top-left and width in device pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) size: i32,
}

impl Placement {
    /// The device rows a layer's spans cover: the image's rows plus one snap row above and
    /// below. [`device_spans`]' output has one entry per row here, starting at `y - 1`.
    pub(crate) fn rows(self) -> usize {
        usize::try_from(self.size.max(0) + 2).unwrap_or(0)
    }
}

/// Writes each source row's hull of texels at or above [`CUTOFF`] into `rows`, from the
/// leftmost to the rightmost. `pixels` is straight-alpha BGRA, `size` x `size`.
pub(crate) fn row_hulls(pixels: &[u8], size: u16, rows: &mut [Span<u16>]) {
    let width = usize::from(size);
    for (row, out) in pixels.chunks_exact(width * 4).zip(rows.iter_mut()) {
        let (pixels, _) = row.as_chunks::<4>();
        let solid = |px: &[u8; 4]| px[3] >= CUTOFF;
        let left = pixels.iter().position(solid);
        let right = pixels.iter().rposition(solid);
        *out = left.zip(right).map(|(l, r)| [column(l), column(r + 1)]);
    }
}

/// Maps source row hulls to device-pixel spans for an image drawn at `at`. A device pixel
/// samples texel k when its centre lies within one texel of k's centre, so each texel span
/// reaches half a texel further out on each side, plus one device pixel for the snap. Spans
/// are clipped to the drawn quad and its snap pixel. `out` gets [`Placement::rows`] entries.
pub(crate) fn device_spans(rows: &[Span<u16>], src: u16, at: Placement, out: &mut [Span<i32>]) {
    out.fill(None);
    if src == 0 || at.size <= 0 {
        return;
    }
    let scale = f64::from(at.size) / f64::from(src);
    let top = at.y - 1;
    let (min_x, max_x) = (at.x - 1, at.x + at.size + 1);
    for (k, hull) in rows.iter().enumerate() {
        let Some([left, right]) = *hull else { continue };
        let k = f64::from(u16::try_from(k).unwrap_or(u16::MAX));
        let reach = |origin: i32, lo: f64, hi: f64| {
            let start = floor(f64::from(origin) + (lo - 0.5) * scale - 0.5) - 1;
            let end = ceil(f64::from(origin) + (hi + 0.5) * scale - 0.5) + 1;
            (start, end)
        };
        let (x0, x1) = reach(at.x, f64::from(left), f64::from(right));
        let (y0, y1) = reach(at.y, k, k + 1.0);
        let span = [x0.max(min_x), x1.min(max_x)];
        for y in y0.max(top)..y1.min(top + i32::try_from(out.len()).unwrap_or(0)) {
            let slot = &mut out[usize::try_from(y - top).unwrap_or(0)];
            *slot = Some(match *slot {
                Some([l, r]) => [l.min(span[0]), r.max(span[1])],
                None => span,
            });
        }
    }
}

fn column(x: usize) -> u16 {
    u16::try_from(x).expect("an image row is at most u16::MAX pixels wide")
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "device coordinates fit i32"
)]
fn floor(v: f64) -> i32 {
    v.floor() as i32
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "device coordinates fit i32"
)]
fn ceil(v: f64) -> i32 {
    v.ceil() as i32
}

#[cfg(test)]
mod tests {
    use super::{CUTOFF, Placement, Span, ceil, device_spans, row_hulls};
    use crate::presence::kumo::frame::FrameState;
    use crate::presence::kumo::look::Look;
    use crate::presence::kumo::raster::Renderer;
    use crate::presence::kumo::shape::Silhouette;

    /// GPUI's filtering as an independent oracle: the bilinear alpha (0..=255) at the centre of
    /// device pixel (x, y) of an image drawn at `at`, clamping at the image's edge.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::many_single_char_names,
        reason = "sample coordinates inside a small test image, in the usual bilinear names"
    )]
    fn sampled(pixels: &[u8], src: u16, at: Placement, x: i32, y: i32) -> f64 {
        let scale = f64::from(at.size) / f64::from(src);
        let n = i32::from(src);
        let alpha = |i: i32, j: i32| {
            let (i, j) = (i.clamp(0, n - 1), j.clamp(0, n - 1));
            f64::from(pixels[usize::try_from(j * n + i).expect("in range") * 4 + 3])
        };
        let u = (f64::from(x - at.x) + 0.5) / scale - 0.5;
        let v = (f64::from(y - at.y) + 0.5) / scale - 0.5;
        let (i, j) = (u.floor(), v.floor());
        let (fu, fv) = (u - i, v - j);
        let (i, j) = (i as i32, j as i32);
        let top = alpha(i, j) * (1.0 - fu) + alpha(i + 1, j) * fu;
        let bottom = alpha(i, j + 1) * (1.0 - fu) + alpha(i + 1, j + 1) * fu;
        top * (1.0 - fv) + bottom * fv
    }

    fn mask(pixels: &[u8], src: u16, at: Placement) -> Vec<Span<i32>> {
        let mut rows = vec![None; usize::from(src)];
        row_hulls(pixels, src, &mut rows);
        let mut out = vec![None; at.rows()];
        device_spans(&rows, src, at, &mut out);
        out
    }

    fn owned(mask: &[Span<i32>], at: Placement, x: i32, y: i32) -> bool {
        usize::try_from(y - (at.y - 1))
            .ok()
            .and_then(|row| mask.get(row).copied().flatten())
            .is_some_and(|[l, r]| (l..r).contains(&x))
    }

    /// Every pixel sampled at or above the cutoff is owned, and each row's span reaches at
    /// most `ceil(scale) + 2` pixels past the sampled-at-cutoff extent of nearby rows.
    fn check(pixels: &[u8], src: u16, at: Placement) {
        let mask = mask(pixels, src, at);
        let scale = f64::from(at.size) / f64::from(src);
        let slack = ceil(scale) + 2;
        let solid: Vec<Vec<i32>> = ((at.y - 1)..=(at.y + at.size))
            .map(|y| {
                (at.x..at.x + at.size)
                    .filter(|&x| {
                        (at.y..at.y + at.size).contains(&y)
                            && sampled(pixels, src, at, x, y) >= f64::from(CUTOFF)
                    })
                    .collect()
            })
            .collect();
        for (row, xs) in solid.iter().enumerate() {
            let y = at.y - 1 + i32::try_from(row).expect("a small image");
            for &x in xs {
                assert!(
                    owned(&mask, at, x, y),
                    "visible pixel {x},{y} not owned at {at:?}"
                );
            }
            let Some([l, r]) = mask[row] else { continue };
            let near = row.saturating_sub(usize::try_from(slack).expect("positive"))
                ..(row + usize::try_from(slack).expect("positive") + 1).min(solid.len());
            let (lo, hi) = solid[near]
                .iter()
                .flatten()
                .fold((i32::MAX, i32::MIN), |(lo, hi), &x| {
                    (lo.min(x), hi.max(x + 1))
                });
            assert!(
                lo <= hi,
                "row {y} is owned with no solid pixel near it at {at:?}"
            );
            assert!(
                l >= lo - slack && r <= hi + slack,
                "row {y}: [{l},{r}) vs [{lo},{hi})"
            );
        }
    }

    fn square(size: u16, alpha: impl Fn(u16, u16) -> u8) -> Vec<u8> {
        let mut px = vec![0; usize::from(size) * usize::from(size) * 4];
        for j in 0..size {
            for i in 0..size {
                px[(usize::from(j) * usize::from(size) + usize::from(i)) * 4 + 3] = alpha(i, j);
            }
        }
        px
    }

    #[test]
    fn the_cutoff_is_inclusive() {
        let mut rows = vec![None; 1];
        row_hulls(&square(1, |_, _| CUTOFF - 1), 1, &mut rows);
        assert_eq!(rows, [None]);
        row_hulls(&square(1, |_, _| CUTOFF), 1, &mut rows);
        assert_eq!(rows, [Some([0, 1])]);
    }

    #[test]
    fn a_hull_spans_the_outermost_solid_texels() {
        let px = square(5, |i, j| if j == 2 && (i == 1 || i == 3) { 200 } else { 0 });
        let mut rows = vec![None; 5];
        row_hulls(&px, 5, &mut rows);
        assert_eq!(rows, [None, None, Some([1, 4]), None, None]);
    }

    #[test]
    fn empty_images_and_placements_own_nothing() {
        let at = Placement {
            x: 10,
            y: 10,
            size: 8,
        };
        assert!(
            mask(&square(4, |_, _| 0), 4, at)
                .iter()
                .all(Option::is_none)
        );
        let mut out = vec![Some([0, 1]); 3];
        device_spans(&[Some([0, 1])], 1, Placement { size: 0, ..at }, &mut out);
        assert!(
            out.iter().all(Option::is_none),
            "a zero-size placement owns nothing"
        );
        assert_eq!(Placement { size: 0, ..at }.rows(), 2);
    }

    #[test]
    fn a_single_solid_texel_owns_its_whole_quad_and_snap_border() {
        let at = Placement {
            x: 4,
            y: 7,
            size: 2,
        };
        let out = mask(&square(1, |_, _| 128), 1, at);
        assert_eq!(out, vec![Some([3, 7]); 4]);
    }

    #[test]
    fn a_soft_ramp_is_owned_wherever_sampling_reaches_the_cutoff() {
        let ramp = square(16, |i, _| u8::try_from(i * 17).expect("at most 255"));
        for size in [16, 20, 24, 32] {
            check(&ramp, 16, Placement { x: 3, y: 5, size });
        }
    }

    #[test]
    fn the_rendered_body_is_owned_at_every_size_and_scale() {
        for src in [48, 84] {
            let renderer = Renderer::new(&Silhouette::emoji(), src, Look::default());
            let mut px = vec![0; renderer.image_len()];
            renderer
                .render(&FrameState::IDLE, &mut px)
                .expect("the buffer is the renderer's size");
            // 1.0, 1.25, 1.5 and 2.0 scale, and 66, a size between 48 and 84 while growing.
            for size in [
                i32::from(src),
                i32::from(src) * 5 / 4,
                i32::from(src) * 3 / 2,
                i32::from(src) * 2,
                66,
            ] {
                check(&px, src, Placement { x: -7, y: 11, size });
            }
        }
    }
}
