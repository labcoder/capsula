//! The neck: Kumo pulled out of the dock line as one drip of its own material. It's narrowest
//! just under the line and swells into the cloud, bends toward the pointer through a lagging
//! spring, and thins as it stretches. Positions are logical pixels of the pull surface, y down.
//! The body is the renderer's own image; this shades only the goo, into a smaller image under
//! it, at whatever scale the caller picks (half the display's, for D1).
#![expect(
    clippy::many_single_char_names,
    reason = "shading math keeps its usual one-letter names"
)]

use super::frame::FrameState;
use super::look::Look;
use super::math::{Vec2, mix, smin, smoothstep, v2, v3};
use super::raster::{finish, surface, to_u8};
use super::shape::{Framing, Silhouette};

/// Capsules along the neck's curve.
const NECK: usize = 10;
/// How far, dock to attachment, the neck stretches before it breaks, whatever the body's size.
pub(crate) const SNAP_LEN: f32 = 144.0;
/// The largest step the spring takes at once, so a stall can't fling the neck.
const MAX_STEP: f32 = 0.1;
/// Goo tiles, pixels: a tile no capsule, fillet or body can reach is cleared without shading.
const TILE: usize = 8;
/// The largest scale the goo is shaded at; above it GPUI scales the image up.
pub(crate) const MAX_SCALE: f32 = 1.0;

/// A rectangle in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rect {
    pub(crate) origin: Vec2,
    pub(crate) size: Vec2,
}

/// A tapered capsule: from, to, and its radius at each end.
type Segment = (Vec2, Vec2, f32, f32);

/// Where the neck is in its life. C1b adds the recoil between the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Attached,
    /// Stretched past [`SNAP_LEN`]: nothing of the neck is left to draw.
    Broken,
}

/// One pull, from the dock line to wherever the body is dragged.
#[derive(Clone, Debug)]
pub(crate) struct Neck {
    /// The centre of the dock line.
    dock: Vec2,
    /// The dragged body's square, logical pixels.
    body_size: f32,
    body: Vec2,
    state: State,
    /// The bend: a control point that follows the neck's middle on a damped spring, so a
    /// sideways drag curves the neck toward the pointer. Kept within [`SNAP_LEN`] of the dock.
    ctrl: Vec2,
    ctrl_v: Vec2,
}

impl Neck {
    pub(crate) fn new(dock: Vec2, body_size: f32) -> Self {
        Self {
            dock,
            body_size,
            body: dock,
            state: State::Attached,
            ctrl: dock,
            ctrl_v: v2(0.0, 0.0),
        }
    }

    pub(crate) fn broken(&self) -> bool {
        self.state == State::Broken
    }

    /// The point on the body the neck holds: just under the crown.
    fn attach(&self) -> Vec2 {
        self.body + v2(0.0, -self.body_size * 0.12)
    }

    /// Moves the body to `body` after `dt` seconds and steps the spring. A negative or
    /// non-finite step counts as none, and a long one as [`MAX_STEP`].
    pub(crate) fn update(&mut self, dt: f32, body: Vec2) {
        let dt = if dt.is_finite() {
            dt.clamp(0.0, MAX_STEP)
        } else {
            0.0
        };
        self.body = body;
        let (a, b) = (self.dock, self.attach());
        let target = scaled(a + b, 0.5) + v2(0.0, (b - a).len() * 0.08);
        // Fixed substeps keep the spring stable at any frame rate.
        let steps: u8 = if dt > 0.0 { 24 } else { 0 };
        let h = dt / 24.0;
        for _ in 0..steps {
            let acc = scaled(target - self.ctrl, 70.0) - scaled(self.ctrl_v, 10.0);
            self.ctrl_v = self.ctrl_v + scaled(acc, h);
            self.ctrl = self.ctrl + scaled(self.ctrl_v, h);
        }
        let reach = self.ctrl - a;
        if reach.len() > SNAP_LEN {
            self.ctrl = a + scaled(reach, SNAP_LEN / reach.len());
        }
        if self.state == State::Attached && (b - a).len() > SNAP_LEN {
            self.state = State::Broken;
        }
    }

    /// How far the neck is stretched toward breaking, 0..=1.
    fn stretch(&self) -> f32 {
        ((self.attach() - self.dock).len() / SNAP_LEN).clamp(0.0, 1.0)
    }

    /// Point `u` (0 at the dock, 1 at the cloud) on the neck's curve.
    fn along(&self, u: f32) -> Vec2 {
        let (a, b, c) = (self.dock, self.attach(), self.ctrl);
        scaled(a, (1.0 - u) * (1.0 - u)) + scaled(c, 2.0 * u * (1.0 - u)) + scaled(b, u * u)
    }

    /// The neck's radius at `u`: a visible stem under the line that thins as it stretches,
    /// swelling smoothly into the cloud.
    fn radius(&self, u: f32) -> f32 {
        let width = self.body_size * 0.73;
        let root = width * mix(0.15, 0.1, self.stretch());
        let len = (self.attach() - self.dock).len().max(1.0);
        let flare = 1.0 - (self.body_size * 0.45 / len).min(0.45);
        mix(root, width * 0.3, smoothstep(flare, 1.0, u))
    }

    /// How the dragged body is scaled about its bottom centre: drawn out toward the dock while
    /// attached, round once broken.
    pub(crate) fn body_scale(&self) -> Vec2 {
        let pull = if self.broken() { 0.0 } else { self.stretch() };
        v2(1.0 - 0.07 * pull, 1.0 + 0.14 * pull)
    }

    fn segments(&self) -> ([Segment; NECK], usize) {
        let mut segs = [(self.dock, self.dock, 0.0, 0.0); NECK];
        if self.broken() {
            return (segs, 0);
        }
        for (i, seg) in segs.iter_mut().enumerate() {
            let (u0, u1) = (fraction(i), fraction(i + 1));
            *seg = (
                self.along(u0),
                self.along(u1),
                self.radius(u0),
                self.radius(u1),
            );
        }
        (segs, NECK)
    }

    /// The region the goo image must cover, or `None` when there's nothing to draw.
    pub(crate) fn bounds(&self) -> Option<Rect> {
        let (segs, n) = self.segments();
        if n == 0 {
            return None;
        }
        let pad = self.body_size * 0.1;
        // The root fillet is an ellipse 2.2 times as wide as it is tall, centred on the dock.
        let root = self.radius(0.0) * 0.9;
        let ends = segs[..n]
            .iter()
            .flat_map(|&(a, b, ra, rb)| [(a, v2(ra, ra)), (b, v2(rb, rb))]);
        let (mut lo, mut hi) = (v2(f32::MAX, f32::MAX), v2(f32::MIN, f32::MIN));
        for (p, r) in ends.chain([(self.dock, v2(root * 2.2, root))]) {
            lo = v2(lo.x.min(p.x - r.x - pad), lo.y.min(p.y - r.y - pad));
            hi = v2(hi.x.max(p.x + r.x + pad), hi.y.max(p.y + r.y + pad));
        }
        Some(Rect {
            origin: v2(lo.x.floor(), lo.y.floor()),
            size: v2((hi.x - lo.x).ceil() + 1.0, (hi.y - lo.y).ceil() + 1.0),
        })
    }

    /// The widest the goo region can get for a body this size, logical pixels: the curve stays
    /// within [`SNAP_LEN`] of the dock while attached, its control point included, and the root
    /// fillet's half-width, at most `2.2 × 0.9 × 0.73 × 0.15` of the body, is under the widest
    /// capsule's radius.
    fn cap(body_size: f32) -> f32 {
        2.0 * (SNAP_LEN + body_size * (0.73 * 0.3 + 0.1)) + 2.0
    }
}

/// `(i / NECK)` as a fraction of the curve.
fn fraction(i: usize) -> f32 {
    f32::from(u8::try_from(i).expect("NECK is small"))
        / f32::from(u8::try_from(NECK).expect("NECK is small"))
}

fn scaled(v: Vec2, s: f32) -> Vec2 {
    v2(v.x * s, v.y * s)
}

/// The goo image didn't fit: a buffer of the wrong size, or a region or scale past the scratch
/// this shader was built for.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the goo image is {width}x{height} with {got} bytes; this shader holds at most {max}x{max}"
)]
pub(crate) struct GooMismatch {
    width: usize,
    height: usize,
    got: usize,
    max: usize,
}

/// Shades the neck. Its scratch is sized once, from the largest region a body this size can
/// need at [`MAX_SCALE`], so shading a frame allocates nothing.
#[derive(Debug)]
pub(crate) struct GooShader {
    max: usize,
    tiles: Vec<(bool, u16)>,
    above: Vec<f32>,
    row: Vec<f32>,
}

impl GooShader {
    pub(crate) fn new(body_size: f32) -> Self {
        let max = pixels(Neck::cap(body_size), MAX_SCALE);
        let tiles = max.div_ceil(TILE);
        Self {
            max,
            tiles: vec![(false, 0); tiles * tiles],
            above: vec![f32::MAX; max],
            row: vec![f32::MAX; max],
        }
    }

    /// The image size, pixels, of region `r` at `scale`.
    pub(crate) fn size(r: Rect, scale: f32) -> (usize, usize) {
        (pixels(r.size.x, scale), pixels(r.size.y, scale))
    }

    /// Shades the goo for region `r` at `scale` pixels per logical pixel into `out`,
    /// straight-alpha BGRA of [`Self::size`], with the body's look and frame so its colours
    /// carry on into the neck. Clear pixels take the body's top colour, never black.
    ///
    /// # Errors
    /// [`GooMismatch`] when `out` isn't the region's size, or the region or scale is larger
    /// than this shader's scratch.
    #[expect(
        clippy::too_many_arguments,
        reason = "the neck, its region, the pixels and the body's shape, look and frame"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "one pass over the pixels, ported whole from the approved spike"
    )]
    pub(crate) fn render(
        &mut self,
        neck: &Neck,
        r: Rect,
        scale: f32,
        shape: &Silhouette,
        look: &Look,
        f: &FrameState,
        out: &mut [u8],
    ) -> Result<(), GooMismatch> {
        let (w, h) = Self::size(r, scale);
        if out.len() != w * h * 4 || w > self.max || h > self.max || scale > MAX_SCALE {
            return Err(GooMismatch {
                width: w,
                height: h,
                got: out.len(),
                max: self.max,
            });
        }
        let top = look.theme.top;
        let clear = [to_u8(top.b), to_u8(top.g), to_u8(top.r), 0];
        for p in out.as_chunks_mut::<4>().0 {
            *p = clear;
        }
        let s = neck.body_size;
        let (segs, n) = neck.segments();
        if n == 0 {
            return Ok(());
        }
        let framing = Framing::CARD;
        let units_per_px = framing.span / s;
        let fillet = s * 0.09;
        let root_r = neck.radius(0.0) * 0.9;
        // Goo glows less than the body it came from.
        let f = &FrameState {
            glow_k: f.glow_k * 0.8,
            ..*f
        };
        let body_reach = s * 0.5 + fillet;
        let squashed = |q: Vec2| {
            v2(
                q.x / f.body_scale.x,
                shape.floor + (q.y - shape.floor - f.body_lift) / f.body_scale.y,
            )
        };
        let to_body = |p: Vec2| {
            framing.center
                + v2(
                    (p.x - neck.body.x) * units_per_px,
                    -(p.y - neck.body.y) * units_per_px,
                )
        };
        let tile = f32::from(u8::try_from(TILE).expect("a small tile"));
        let px = 1.0 / scale;
        let reach = tile * px * 0.75 + s * 0.06;
        let seg_reach = tile * px * 0.75 + s * 0.15;
        let tw = w.div_ceil(TILE);
        for (t, slot) in self.tiles[..tw * h.div_ceil(TILE)].iter_mut().enumerate() {
            let c = r.origin
                + scaled(
                    v2(
                        index_f32((t % tw) * TILE) + tile * 0.5,
                        index_f32((t / tw) * TILE) + tile * 0.5,
                    ),
                    px,
                );
            let mut mask = 0_u16;
            for (k, &(a, b, ra, rb)) in segs[..n].iter().enumerate() {
                if tapered(c, a, b, ra, rb) < seg_reach {
                    mask |= 1 << k;
                }
            }
            let busy = mask != 0
                || (c - neck.body).len() < body_reach + reach
                || (c - neck.dock).len() - root_r * 2.2 < reach;
            *slot = (busy, mask);
        }
        let (above, row) = (&mut self.above[..w], &mut self.row[..w]);
        row.fill(f32::MAX);
        for j in 0..h {
            above.copy_from_slice(row);
            row.fill(f32::MAX);
            for i in 0..w {
                let (busy, mask) = self.tiles[(j / TILE) * tw + i / TILE];
                if !busy {
                    continue;
                }
                let o = &mut out[(j * w + i) * 4..][..4];
                let p = r.origin + v2((index_f32(i) + 0.5) * px, (index_f32(j) + 0.5) * px);
                let q = to_body(p);
                let near_body = (p - neck.body).len() < body_reach;
                let neck_at = |p: Vec2| {
                    let mut d = f32::MAX;
                    for (k, &(a, b, ra, rb)) in segs[..n].iter().enumerate() {
                        if mask & (1 << k) != 0 {
                            d = d.min(tapered(p, a, b, ra, rb));
                        }
                    }
                    // A soft fillet where the neck leaves the line, so it grows out of the dock.
                    let rel = p - neck.dock;
                    smin(d, v2(rel.x / 2.2, rel.y).len() - root_r, s * 0.05)
                };
                let body_at = |p: Vec2| {
                    let q = squashed(to_body(p));
                    shape.sdf(v3(q.x, q.y, 0.0)) / units_per_px
                };
                let goo_at = |p: Vec2| {
                    let d = neck_at(p);
                    if !near_body || d > fillet * 1.5 {
                        return d;
                    }
                    smin(d, body_at(p), fillet)
                };
                let mut d = neck_at(p);
                // Inside the body the neck carries on over it, soft-edged and fading with depth,
                // so neck and crown read as one surface rather than a crown on a funnel.
                let mut over_body = None;
                // The smooth goo field, which normals come from; `d` is what's drawn.
                let mut field = d;
                if near_body && d <= fillet * 1.5 {
                    let b = body_at(p);
                    field = smin(d, b, fillet);
                    if b < -2.0 * px {
                        let fade = 1.0 - smoothstep(0.0, s * 0.16, -b);
                        // Where the neck has faded the pixel stays clear but is still shaded, so
                        // the scaled image's edge blends toward the body's colour.
                        over_body = Some(if fade <= 0.0 || d >= 0.0 {
                            0.0
                        } else {
                            fade * (-d / (s * 0.06)).min(1.0)
                        });
                    } else {
                        d = field;
                    }
                }
                row[i] = field;
                let cov = over_body.unwrap_or((0.5 - d / px).clamp(0.0, 1.0));
                let mut color = top;
                if cov > 0.0 || over_body.is_some() {
                    let u = (-d / (s * 0.12)).clamp(0.0, 1.0);
                    let nxy = if u < 1.0 {
                        let g = v2(
                            if i > 0 && row[i - 1] < f32::MAX {
                                field - row[i - 1]
                            } else {
                                goo_at(p + v2(px, 0.0)) - field
                            },
                            if above[i] < f32::MAX {
                                field - above[i]
                            } else {
                                goo_at(p + v2(0.0, px)) - field
                            },
                        );
                        scaled(g, (1.0 - u) / g.len().max(1e-6))
                    } else {
                        v2(0.0, 0.0)
                    };
                    let nz = (1.0 - nxy.dot(nxy)).max(0.0).sqrt();
                    // Screen y is down; body space is y up.
                    let normal = v3(nxy.x, -nxy.y, nz);
                    // Goo takes the colour of the body's middle, so the neck is the same stuff.
                    let qc = v2(q.x.clamp(-0.6, 0.6), q.y.clamp(shape.floor, 0.1));
                    let lit = surface(look, normal, 0.35 + u * 0.4, 1.0, qc, shape.floor);
                    color = finish(&look.theme, f, &lit, qc);
                }
                o.copy_from_slice(&[to_u8(color.b), to_u8(color.g), to_u8(color.r), to_u8(cov)]);
            }
        }
        Ok(())
    }
}

/// Pixels covering `logical` at `scale`.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a clamped, small pixel count"
)]
fn pixels(logical: f32, scale: f32) -> usize {
    (logical * scale).ceil().clamp(0.0, 65_535.0) as usize
}

#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "a pixel index in a small image"
)]
fn index_f32(i: usize) -> f32 {
    i as f32
}

/// Distance to a capsule whose radius tapers from `ra` at `a` to `rb` at `b`.
fn tapered(p: Vec2, a: Vec2, b: Vec2, ra: f32, rb: f32) -> f32 {
    let ba = b - a;
    let l2 = ba.dot(ba);
    if l2 < 1e-6 {
        return (p - a).len() - ra.max(rb);
    }
    let t = ((p - a).dot(ba) / l2).clamp(0.0, 1.0);
    (p - (a + scaled(ba, t))).len() - mix(ra, rb, t)
}

#[cfg(test)]
mod tests {
    use super::{GooMismatch, GooShader, MAX_SCALE, Neck, Rect, SNAP_LEN, pixels, tapered};
    use crate::presence::kumo::frame::FrameState;
    use crate::presence::kumo::look::Look;
    use crate::presence::kumo::math::v2;
    use crate::presence::kumo::shape::Silhouette;

    const FRAME: f32 = 1.0 / 72.0;
    const BODY: f32 = 84.0;

    /// A neck pulled straight down until its attachment is `len` below the dock.
    fn pulled(len: f32) -> Neck {
        let dock = v2(400.0, 10.0);
        let mut neck = Neck::new(dock, BODY);
        let body = v2(dock.x, dock.y + len + BODY * 0.12);
        for _ in 0..72 {
            neck.update(FRAME, body);
        }
        neck
    }

    fn shade(neck: &Neck, shader: &mut GooShader) -> (usize, usize, Vec<u8>) {
        let r = neck.bounds().expect("an attached neck has goo");
        shade_in(neck, r, MAX_SCALE, shader)
    }

    fn shade_in(
        neck: &Neck,
        r: Rect,
        scale: f32,
        shader: &mut GooShader,
    ) -> (usize, usize, Vec<u8>) {
        let (w, h) = GooShader::size(r, scale);
        let mut out = vec![0; w * h * 4];
        shader
            .render(
                neck,
                r,
                scale,
                &Silhouette::emoji(),
                &Look::default(),
                &FrameState::IDLE,
                &mut out,
            )
            .expect("the region fits the shader");
        (w, h, out)
    }

    #[test]
    fn the_neck_breaks_one_unit_past_the_snap_length() {
        let at = pulled(SNAP_LEN);
        assert!(!at.broken(), "attached at exactly {SNAP_LEN}");
        assert!(at.bounds().is_some());
        let past = pulled(SNAP_LEN + 1.0);
        assert!(past.broken(), "broken one unit past");
        assert_eq!(past.bounds(), None, "and nothing is left to draw");
        assert_eq!(past.body_scale(), v2(1.0, 1.0), "the body is round again");
    }

    #[test]
    fn a_stretched_neck_draws_the_body_out() {
        let neck = pulled(SNAP_LEN);
        let s = neck.body_scale();
        assert!(
            (s.x - 0.93).abs() < 1e-4 && (s.y - 1.14).abs() < 1e-4,
            "{s:?}"
        );
        assert_eq!(pulled(0.0).body_scale(), v2(1.0, 1.0), "slack, it's round");
    }

    #[test]
    fn odd_steps_never_fling_or_poison_the_spring() {
        let mut neck = pulled(100.0);
        let body = v2(400.0 + 60.0, 10.0 + 100.0);
        for dt in [f32::NAN, f32::INFINITY, -1.0, 1.0e9] {
            neck.update(dt, body);
            let r = neck.bounds().expect("still attached");
            assert!(
                r.origin.x.is_finite() && r.size.x.is_finite(),
                "{dt}: {r:?}"
            );
            assert!(
                r.size.x <= Neck::cap(BODY) && r.size.y <= Neck::cap(BODY),
                "{dt}: {r:?}"
            );
        }
    }

    #[test]
    fn the_region_stays_within_the_cap_through_a_fast_swing() {
        let dock = v2(400.0, 10.0);
        let mut neck = Neck::new(dock, BODY);
        // Whip the body side to side at the edge of the snap length.
        for k in 0..288_u16 {
            let side = if (k / 6) % 2 == 0 { 1.0 } else { -1.0 };
            neck.update(FRAME, dock + v2(side * 90.0, 100.0));
            let r = neck.bounds().expect("still attached");
            assert!(
                r.size.x <= Neck::cap(BODY) && r.size.y <= Neck::cap(BODY),
                "frame {k}: {r:?}"
            );
        }
    }

    #[test]
    fn a_short_sideways_pull_keeps_the_whole_root_in_the_region() {
        let dock = v2(400.0, 10.0);
        let mut neck = Neck::new(dock, BODY);
        neck.update(0.0, v2(420.0, 20.08));
        let scale = 0.5;
        let r = neck.bounds().expect("attached");
        // Shade a wider image on the same pixel grid; nothing may land outside the region.
        let grow = 40.0;
        let wide = Rect {
            origin: r.origin - v2(grow, grow),
            size: r.size + v2(2.0 * grow, 2.0 * grow),
        };
        let mut shader = GooShader::new(BODY);
        let (w, h) = GooShader::size(r, scale);
        let (ww, wh, out) = shade_in(&neck, wide, scale, &mut shader);
        let skip = pixels(grow, scale);
        let clipped: Vec<(usize, usize, u8)> = (0..wh)
            .flat_map(|j| (0..ww).map(move |i| (i, j)))
            .filter(|&(i, j)| !((skip..skip + w).contains(&i) && (skip..skip + h).contains(&j)))
            .map(|(i, j)| (i, j, out[(j * ww + i) * 4 + 3]))
            .filter(|&(.., a)| a > 0)
            .collect();
        assert!(clipped.is_empty(), "{r:?} clips {clipped:?}");
    }

    #[test]
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a point inside a small test image"
    )]
    fn the_goo_covers_its_middle_and_clears_its_corners() {
        let neck = pulled(100.0);
        let mut shader = GooShader::new(BODY);
        let (w, h, out) = shade(&neck, &mut shader);
        let middle = neck.along(0.5);
        let r = neck.bounds().expect("goo");
        let i = ((middle.x - r.origin.x) * MAX_SCALE) as usize;
        let j = ((middle.y - r.origin.y) * MAX_SCALE) as usize;
        assert_eq!(out[(j * w + i) * 4 + 3], 255, "the neck's middle is solid");
        for (ci, cj) in [(0, 0), (w - 1, 0)] {
            assert_eq!(out[(cj * w + ci) * 4 + 3], 0, "corner {ci},{cj} is clear");
        }
        assert!(h > 100, "the region spans the neck");
    }

    #[test]
    fn no_goo_pixel_is_black_clear_or_covered() {
        let neck = pulled(100.0);
        let mut shader = GooShader::new(BODY);
        let (_, _, out) = shade(&neck, &mut shader);
        let px = out.as_chunks::<4>().0;
        assert!(px.iter().any(|p| p[3] == 255), "some goo is solid");
        assert!(px.iter().any(|p| p[3] == 0), "some goo is clear");
        let black = px.iter().filter(|p| p[..3] == [0, 0, 0]).count();
        assert_eq!(
            black, 0,
            "a scaled edge blends toward these, so none is black"
        );
    }

    #[test]
    fn shading_twice_gives_the_same_bytes_and_keeps_the_scratch() {
        let neck = pulled(100.0);
        let mut shader = GooShader::new(BODY);
        let tiles = shader.tiles.as_ptr();
        let rows = (shader.above.as_ptr(), shader.row.as_ptr());
        let first = shade(&neck, &mut shader).2;
        assert_eq!(shade(&neck, &mut shader).2, first);
        assert_eq!(shader.tiles.as_ptr(), tiles, "the tile table is reused");
        assert_eq!(
            (shader.above.as_ptr(), shader.row.as_ptr()),
            rows,
            "the rows are reused"
        );
    }

    #[test]
    fn a_wrong_buffer_or_scale_is_an_error() {
        let neck = pulled(100.0);
        let r = neck.bounds().expect("goo");
        let mut shader = GooShader::new(BODY);
        let (w, h) = GooShader::size(r, MAX_SCALE);
        let mut short = vec![0; w * h * 4 - 1];
        let render = |shader: &mut GooShader, scale: f32, out: &mut [u8]| {
            shader.render(
                &neck,
                r,
                scale,
                &Silhouette::emoji(),
                &Look::default(),
                &FrameState::IDLE,
                out,
            )
        };
        assert_eq!(
            render(&mut shader, MAX_SCALE, &mut short),
            Err(GooMismatch {
                width: w,
                height: h,
                got: w * h * 4 - 1,
                max: shader.max
            })
        );
        let (w2, h2) = GooShader::size(r, 2.0);
        let mut big = vec![0; w2 * h2 * 4];
        assert!(
            render(&mut shader, 2.0, &mut big).is_err(),
            "past the scratch's scale"
        );
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "small exact distances")]
    fn a_capsule_distance_is_exact_on_and_off_its_axis() {
        let (a, b) = (v2(0.0, 0.0), v2(10.0, 0.0));
        assert_eq!(tapered(v2(5.0, 0.0), a, b, 2.0, 2.0), -2.0);
        assert_eq!(tapered(v2(5.0, 3.0), a, b, 2.0, 2.0), 1.0);
        assert_eq!(tapered(v2(5.0, 3.0), a, b, 1.0, 3.0), 1.0);
        assert_eq!(tapered(v2(13.0, 0.0), a, b, 2.0, 2.0), 1.0);
        assert_eq!(
            tapered(v2(0.0, 4.0), a, a, 1.0, 3.0),
            1.0,
            "a point capsule"
        );
    }
}
