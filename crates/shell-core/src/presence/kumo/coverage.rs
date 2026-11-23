//! Which device pixels of a drawn Kumo image take the pointer: a conservative mask. GPUI scales
//! each image with bilinear filtering, so this owns every device pixel whose sample can see a
//! texel at or above the cutoff, plus one pixel for snapping. It never lets a click through a
//! visible pixel, and over-owns a band about one source texel plus three device pixels wide.

/// The per-layer alpha cutoff. Two layers each under 75/255 composite to under 128/255, so a
/// pixel that is visible, at 128 or more after compositing, has a texel at 75 or more in one of
/// its layers' sampling footprints.
pub(crate) const CUTOFF: u8 = 75;

/// A half-open span of columns, `[left, right)`.
pub type Span<T> = Option<[T; 2]>;

/// Where GPUI draws an image: its snapped device bounds. GPUI rounds each edge on its own, so
/// a square image at a fractional origin can land a pixel wider than it is tall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Placement {
    /// The device rows a layer's spans cover: the image's rows plus one snap row above and
    /// below. [`device_spans`]' output has one entry per row here, starting at `y - 1`.
    pub fn rows(self) -> usize {
        usize::try_from(self.height.max(0) + 2).unwrap_or(0)
    }
}

/// Writes each source row's hull of texels at or above [`CUTOFF`] into `rows`, from the
/// leftmost to the rightmost. `pixels` is straight-alpha BGRA, `size` texels wide: the body's
/// square or the neck's rectangle.
pub fn row_hulls(pixels: &[u8], size: u16, rows: &mut [Span<u16>]) {
    rows.fill(None);
    let width = usize::from(size);
    if width == 0 {
        return;
    }
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
/// are clipped to the drawn quad and its snap pixel. The source is `width` texels wide and one
/// texel tall per row hull. `out` gets [`Placement::rows`] entries.
pub fn device_spans(rows: &[Span<u16>], width: u16, at: Placement, out: &mut [Span<i32>]) {
    out.fill(None);
    let height = u16::try_from(rows.len()).unwrap_or(u16::MAX);
    if width == 0 || height == 0 || at.width <= 0 || at.height <= 0 {
        return;
    }
    let (scale_x, scale_y) = (
        f64::from(at.width) / f64::from(width),
        f64::from(at.height) / f64::from(height),
    );
    let top = at.y - 1;
    let (min_x, max_x) = (at.x - 1, at.x + at.width + 1);
    for (k, hull) in rows.iter().enumerate() {
        let Some([left, right]) = *hull else { continue };
        let k = f64::from(u16::try_from(k).unwrap_or(u16::MAX));
        let reach = |origin: i32, scale: f64, lo: f64, hi: f64| {
            let start = floor(f64::from(origin) + (lo - 0.5) * scale - 0.5) - 1;
            let end = ceil(f64::from(origin) + (hi + 0.5) * scale - 0.5) + 1;
            (start, end)
        };
        let (x0, x1) = reach(at.x, scale_x, f64::from(left), f64::from(right));
        let (y0, y1) = reach(at.y, scale_y, k, k + 1.0);
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
    use super::{CUTOFF, Placement, Span, device_spans, row_hulls};
    use crate::presence::kumo::frame::FrameState;
    use crate::presence::kumo::look::Look;
    use crate::presence::kumo::math::v2;
    use crate::presence::kumo::neck::{GooShader, Neck, Piece, Rect};
    use crate::presence::kumo::raster::Renderer;
    use crate::presence::kumo::shape::Silhouette;

    /// One drawn image: straight-alpha BGRA pixels, `w` x `h`, at `at`.
    #[derive(Clone, Copy)]
    struct Layer<'a> {
        pixels: &'a [u8],
        w: u16,
        h: u16,
        at: Placement,
    }

    /// GPUI's `snap_bounds`, written out on its own: an image laid out at logical `left, top`
    /// with side `side` has each device edge rounded half toward zero at `scale`.
    fn snapped(left: f64, top: f64, side: f64, scale: f64) -> Placement {
        snapped_rect(left, top, side, side, scale)
    }

    /// [`snapped`] for a `width` x `height` rectangle.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::float_cmp,
        reason = "small device coordinates, and GPUI tests for an exact half"
    )]
    fn snapped_rect(left: f64, top: f64, width: f64, height: f64, scale: f64) -> Placement {
        let edge = |v: f64| {
            let v = v * scale;
            (if v.fract().abs() == 0.5 {
                v.trunc()
            } else {
                v.round()
            }) as i32
        };
        let (x, y) = (edge(left), edge(top));
        Placement {
            x,
            y,
            width: edge(left + width) - x,
            height: edge(top + height) - y,
        }
    }

    /// GPUI's filtering as an independent oracle: the bilinear alpha (0..=255) at the centre of
    /// device pixel (x, y), clamping at the image's edge, and 0 outside the drawn quad.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::many_single_char_names,
        reason = "sample coordinates inside a small test image, in the usual bilinear names"
    )]
    fn sampled(l: Layer<'_>, x: i32, y: i32) -> f64 {
        let at = l.at;
        if !(at.x..at.x + at.width).contains(&x) || !(at.y..at.y + at.height).contains(&y) {
            return 0.0;
        }
        let (w, h) = (i32::from(l.w), i32::from(l.h));
        let alpha = |i: i32, j: i32| {
            let (i, j) = (i.clamp(0, w - 1), j.clamp(0, h - 1));
            f64::from(l.pixels[usize::try_from(j * w + i).expect("in range") * 4 + 3])
        };
        let u = (f64::from(x - at.x) + 0.5) * f64::from(w) / f64::from(at.width) - 0.5;
        let v = (f64::from(y - at.y) + 0.5) * f64::from(h) / f64::from(at.height) - 0.5;
        let (i, j) = (u.floor(), v.floor());
        let (fu, fv) = (u - i, v - j);
        let (i, j) = (i as i32, j as i32);
        let top = alpha(i, j) * (1.0 - fu) + alpha(i + 1, j) * fu;
        let bottom = alpha(i, j + 1) * (1.0 - fu) + alpha(i + 1, j + 1) * fu;
        top * (1.0 - fv) + bottom * fv
    }

    /// Source-over composite of every layer at (x, y), rounded to the nearest byte, is 128 or more.
    fn visible(layers: &[Layer<'_>], x: i32, y: i32) -> bool {
        let alpha = layers.iter().fold(0.0, |a, &l| {
            let s = sampled(l, x, y) / 255.0;
            s + a * (1.0 - s)
        });
        alpha * 255.0 >= 127.5
    }

    fn mask(l: Layer<'_>) -> Vec<Span<i32>> {
        let mut rows = vec![None; usize::from(l.h)];
        row_hulls(l.pixels, l.w, &mut rows);
        let mut out = vec![None; l.at.rows()];
        device_spans(&rows, l.w, l.at, &mut out);
        out
    }

    fn owned(mask: &[Span<i32>], at: Placement, x: i32, y: i32) -> bool {
        usize::try_from(y - (at.y - 1))
            .ok()
            .and_then(|row| mask.get(row).copied().flatten())
            .is_some_and(|[l, r]| (l..r).contains(&x))
    }

    /// Asserts every visible pixel is owned by some layer's mask, and returns the excess: the
    /// furthest any span's end pixel lies from a visible pixel, in device pixels. Pixels inside
    /// a span, between its ends, are the permitted hull gaps and are not measured.
    fn check(layers: &[Layer<'_>]) -> f64 {
        let masks: Vec<_> = layers.iter().map(|&l| mask(l)).collect();
        let x0 = layers.iter().map(|l| l.at.x - 2).min().unwrap_or(0);
        let x1 = layers
            .iter()
            .map(|l| l.at.x + l.at.width + 2)
            .max()
            .unwrap_or(0);
        let y0 = layers.iter().map(|l| l.at.y - 2).min().unwrap_or(0);
        let y1 = layers
            .iter()
            .map(|l| l.at.y + l.at.height + 2)
            .max()
            .unwrap_or(0);
        let mut seen = Vec::new();
        for y in y0..y1 {
            let xs: Vec<i32> = (x0..x1).filter(|&x| visible(layers, x, y)).collect();
            for &x in &xs {
                let by = layers.iter().zip(&masks).any(|(l, m)| owned(m, l.at, x, y));
                assert!(by, "visible pixel {x},{y} is not owned");
            }
            if let (Some(&l), Some(&r)) = (xs.first(), xs.last()) {
                seen.push((y, l, r));
            }
        }
        let distance = |x: i32, y: i32| {
            seen.iter()
                .map(|&(vy, l, r)| {
                    let dx = (l - x).max(x - r).max(0);
                    f64::from(dx.max((vy - y).abs()))
                })
                .fold(f64::INFINITY, f64::min)
        };
        let mut excess: f64 = 0.0;
        for (l, m) in layers.iter().zip(&masks) {
            for (row, span) in m.iter().enumerate() {
                let Some([a, b]) = *span else { continue };
                let y = l.at.y - 1 + i32::try_from(row).expect("a small image");
                excess = excess.max(distance(a, y)).max(distance(b - 1, y));
            }
        }
        excess
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

    fn body(src: u16, f: &FrameState) -> Vec<u8> {
        let renderer = Renderer::new(&Silhouette::emoji(), src, Look::default());
        let mut px = vec![0; renderer.image_len()];
        renderer
            .render(f, &mut px)
            .expect("the buffer is the renderer's size");
        px
    }

    /// How far, in source texels, alpha climbs from 75 to 128 going in from the body's left
    /// edge along its middle row: the width of the material's soft edge.
    fn soft_edge(px: &[u8], src: u16) -> f64 {
        let row = usize::from(src / 2) * usize::from(src);
        let alpha = |i: usize| f64::from(px[(row + i) * 4 + 3]);
        let crossing = |t: f64| {
            let i = (1..usize::from(src))
                .find(|&i| alpha(i) >= t)
                .expect("the middle row reaches the cutoff");
            let i_f = f64::from(u16::try_from(i).expect("a small image"));
            i_f - 1.0 + (t - alpha(i - 1)) / (alpha(i) - alpha(i - 1))
        };
        crossing(128.0) - crossing(f64::from(CUTOFF))
    }

    #[test]
    fn the_cutoff_is_inclusive() {
        let mut rows = vec![None; 1];
        row_hulls(&square(1, |_, _| 74), 1, &mut rows);
        assert_eq!(rows, [None]);
        row_hulls(&square(1, |_, _| 75), 1, &mut rows);
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
            width: 8,
            height: 8,
        };
        let clear = square(4, |_, _| 0);
        let layer = Layer {
            pixels: &clear,
            w: 4,
            h: 4,
            at,
        };
        assert!(mask(layer).iter().all(Option::is_none));
        let mut out = vec![Some([0, 1]); 3];
        let flat = Placement { height: 0, ..at };
        device_spans(&[Some([0, 1])], 1, flat, &mut out);
        assert!(
            out.iter().all(Option::is_none),
            "a zero-height placement owns nothing"
        );
        assert_eq!(flat.rows(), 2);
        let mut stale = [Some([0, 1]); 2];
        row_hulls(&[], 0, &mut stale);
        assert_eq!(stale, [None, None]);
    }

    #[test]
    fn a_single_solid_texel_owns_its_whole_quad_and_snap_border() {
        let px = square(1, |_, _| 128);
        let at = Placement {
            x: 4,
            y: 7,
            width: 2,
            height: 3,
        };
        let out = mask(Layer {
            pixels: &px,
            w: 1,
            h: 1,
            at,
        });
        assert_eq!(out, vec![Some([3, 7]); 5]);
    }

    #[test]
    fn a_wide_source_scales_each_axis_by_its_own_size() {
        let at = Placement {
            x: 0,
            y: 0,
            width: 12,
            height: 15,
        };
        let mut out = [None; 17];
        device_spans(&[None, Some([0, 1]), None], 2, at, &mut out);
        assert_eq!(out[..2], [None; 2]);
        assert_eq!(out[2..14], [Some([-1, 10]); 12]);
        assert_eq!(out[14..], [None; 3]);

        let mut full = [None; 4];
        let flat = Placement {
            width: 4,
            height: 2,
            ..at
        };
        device_spans(&[Some([0, 2])], 2, flat, &mut full);
        assert_eq!(full, [Some([-1, 5]); 4]);
    }

    #[test]
    fn two_layers_under_the_cutoff_never_composite_visible() {
        let at = Placement {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        };
        for (a, b, seen) in [
            (74, 74, false),
            (75, 75, true),
            (102, 102, true),
            (127, 0, false),
            (128, 0, true),
        ] {
            let (pa, pb) = (square(4, |_, _| a), square(4, |_, _| b));
            let layers = [
                Layer {
                    pixels: &pa,
                    w: 4,
                    h: 4,
                    at,
                },
                Layer {
                    pixels: &pb,
                    w: 4,
                    h: 4,
                    at: Placement { x: 3, y: 2, ..at },
                },
            ];
            assert_eq!(visible(&layers, 4, 4), seen, "{a} over {b}");
            check(&layers);
        }
    }

    /// The over-ownership bound per axis: a hull reaches a whole texel past its outermost texel
    /// at or above the cutoff, plus a floor or ceil and the snap pixel, and the visible edge
    /// starts within the soft edge `d` of that texel, plus a pixel to the next centre.
    fn band(d: f64, l: Layer<'_>) -> f64 {
        let x = f64::from(l.at.width) / f64::from(l.w);
        let y = f64::from(l.at.height) / f64::from(l.h);
        (d + 1.0) * x.max(y) + 3.0
    }

    #[test]
    fn the_rendered_body_is_owned_within_the_soft_edge_band() {
        for (src, side, soft) in [
            (48, 48.0, 286),
            (84, 84.0, 340),
            (84, 66.0, 340),
            (168, 84.0, 457),
        ] {
            let px = body(src, &FrameState::IDLE);
            let d = soft_edge(&px, src);
            assert!(
                (d * 1000.0 - f64::from(soft)).abs() < 0.5,
                "the soft edge at {src} px is {d}"
            );
            for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
                for (left, top) in [(-7.0, 11.0), (10.3, 7.6), (0.2, 0.6), (-3.25, 12.5)] {
                    let at = snapped(left, top, side, scale);
                    let layer = Layer {
                        pixels: &px,
                        w: src,
                        h: src,
                        at,
                    };
                    let excess = check(&[layer]);
                    assert!(
                        excess <= band(d, layer),
                        "excess {excess} at {src} px, {at:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn two_bodies_a_frame_apart_are_owned() {
        let px = body(84, &FrameState::IDLE);
        let first = snapped(10.3, 7.6, 84.0, 1.25);
        let layers = [
            Layer {
                pixels: &px,
                w: 84,
                h: 84,
                at: first,
            },
            Layer {
                pixels: &px,
                w: 84,
                h: 84,
                at: snapped(13.7, 6.1, 84.0, 1.25),
            },
        ];
        let excess = check(&layers);
        assert!(
            excess <= band(soft_edge(&px, 84), layers[0]),
            "excess {excess}"
        );
    }

    const BODY: f32 = 84.0;

    /// The goo for `neck` shaded at `scale`: its pixels, size, and region in logical pixels.
    fn goo(neck: &Neck, scale: f32) -> (Vec<u8>, u16, u16, Rect) {
        let r = neck.bounds(Piece::Neck).expect("an attached neck has goo");
        let (w, h) = GooShader::size(r, scale);
        let mut px = vec![0; w * h * 4];
        let f = FrameState {
            body_scale: neck.body_scale(),
            ..FrameState::IDLE
        };
        GooShader::neck(BODY)
            .render(
                neck,
                Piece::Neck,
                r,
                scale,
                &Silhouette::emoji(),
                &Look::default(),
                &f,
                &mut px,
            )
            .expect("the region fits the shader");
        let size = |n: usize| u16::try_from(n).expect("a small image");
        (px, size(w), size(h), r)
    }

    /// The image's widest soft edge in texels: the furthest any texel at or above the cutoff
    /// lies, as a chessboard distance, from one at 128 or more.
    fn transition(l: Layer<'_>) -> f64 {
        let (w, h) = (i32::from(l.w), i32::from(l.h));
        let alpha = |i: i32, j: i32| {
            if (0..w).contains(&i) && (0..h).contains(&j) {
                l.pixels[usize::try_from(j * w + i).expect("in range") * 4 + 3]
            } else {
                0
            }
        };
        let mut worst = 0;
        for j in 0..h {
            for i in 0..w {
                if alpha(i, j) < CUTOFF {
                    continue;
                }
                let near = (0..=w.max(h)).find(|&k| {
                    (-k..=k).any(|d| {
                        [
                            (i + d, j - k),
                            (i + d, j + k),
                            (i - k, j + d),
                            (i + k, j + d),
                        ]
                        .into_iter()
                        .any(|(a, b)| alpha(a, b) >= 128)
                    })
                });
                worst = worst.max(near.unwrap_or(i32::MAX));
            }
        }
        f64::from(worst)
    }

    /// The neck's own images, half and full resolution at several DPIs and with a fractional
    /// window offset, alone and joined to the body: every visible pixel is owned, and the
    /// over-ownership stays within the band of the image's measured soft edge.
    #[test]
    fn the_neck_and_its_join_are_owned_within_the_soft_edge_band() {
        let dock = v2(400.3, 10.6);
        let pull = |to: (f32, f32), frames: u16| {
            let body = dock + v2(to.0, to.1);
            let mut neck = Neck::new(dock, BODY);
            for _ in 0..frames {
                neck.update(1.0 / 72.0, body);
            }
            (neck, body)
        };
        let mut widest: f64 = 0.0;
        for (to, frames) in [
            ((0.0, 70.08), 72),
            ((0.0, 150.08), 72),
            ((70.0, 90.0), 6),
            ((20.0, 10.08), 72),
        ] {
            let (neck, centre) = pull(to, frames);
            // The body drawn with the same frame as the goo, drawn out as the neck stretches.
            let f = FrameState {
                body_scale: neck.body_scale(),
                ..FrameState::IDLE
            };
            let bpx = body(84, &f);
            for dpi in [1.0_f32, 1.25, 1.5, 2.0] {
                for scale in [dpi * 0.5, dpi.min(1.0)] {
                    let (px, w, h, r) = goo(&neck, scale);
                    assert!(px.as_chunks::<4>().0.iter().any(|p| p[3] == 255), "{to:?}");
                    for (ox, oy) in [(0.0, 0.0), (0.37, 0.71)] {
                        let neck_l = Layer {
                            pixels: &px,
                            w,
                            h,
                            at: snapped_rect(
                                f64::from(r.origin.x) + ox,
                                f64::from(r.origin.y) + oy,
                                f64::from(w) / f64::from(scale),
                                f64::from(h) / f64::from(scale),
                                f64::from(dpi),
                            ),
                        };
                        let body_l = Layer {
                            pixels: &bpx,
                            w: 84,
                            h: 84,
                            at: snapped(
                                f64::from(centre.x) - 42.0 + ox,
                                f64::from(centre.y) - 42.0 + oy,
                                84.0,
                                f64::from(dpi),
                            ),
                        };
                        let soft = transition(neck_l);
                        widest = widest.max(soft);
                        let alone = check(&[neck_l]);
                        assert!(alone <= band(soft, neck_l), "{to:?} at {dpi}x: {alone}");
                        let join = check(&[body_l, neck_l]);
                        let limit = band(soft, neck_l).max(band(soft_edge(&bpx, 84), body_l));
                        assert!(join <= limit, "{to:?} joined at {dpi}x: {join}");
                    }
                }
            }
        }
        assert!(widest <= 6.0, "the neck's soft edge is {widest} texels");
    }
}
