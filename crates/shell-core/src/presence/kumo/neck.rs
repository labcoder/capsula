//! The neck: Kumo pulled out of the dock line as one drip of its own material. It's narrowest
//! just under the line and swells into the cloud, bends toward the pointer through a lagging
//! spring, and thins as it stretches. Past the snap length it breaks near the dock: the dock's
//! part retracts as a short drip, the cloud's part melts back into the cloud, and the body
//! springs back to round. Positions are logical pixels of the pull surface, y down. The body is
//! the renderer's own image; this shades only the goo, piece by piece, into smaller images
//! under it, at whatever scale the caller picks (half the display's, for D1).
#![expect(
    clippy::many_single_char_names,
    reason = "shading math keeps its usual one-letter names"
)]

use super::expression::Motion;
use super::frame::FrameState;
use super::look::Look;
use super::math::{Vec2, mix, smin, smoothstep, v2, v3};
use super::raster::{finish, surface, to_u8};
use super::shape::{Framing, Silhouette};

/// Capsules along a piece of the neck's curve.
const NECK: usize = 10;
/// How far, dock to attachment, the neck stretches before it breaks, whatever the body's size.
pub const SNAP_LEN: f32 = 144.0;
/// The largest step the spring takes at once, so a stall can't fling the neck.
const MAX_STEP: f32 = 0.1;
/// Seconds from the break until the drip has gone back into the line and the body is round.
const RECOIL_SECS: f32 = 0.32;
/// Seconds the stub takes to melt back into the cloud.
const STUB_SECS: f32 = 0.12;
/// Where along the curve (0 at the dock, 1 at the cloud) it breaks: near the dock, where it's
/// thinnest. The drip is the curve up to here.
const BREAK_AT: f32 = 0.16;
/// Where the stub starts, just past the break, so nothing is drawn between the two pieces.
const STUB_FROM: f32 = BREAK_AT + 0.05;
/// The fillet where the goo melts into the body, as a fraction of the body. The shader blends in
/// the body's field within 1.5 fillets of a capsule.
const FILLET: f32 = 0.09;
/// How softly the root's ellipse blends into the neck's capsules, as a fraction of the body.
/// The blend dips at most a quarter of it below either term.
const ROOT_BLEND: f32 = 0.05;
/// How far from the body's centre the goo blends into it, as a fraction of the body.
const BODY_REACH: f32 = 0.5 + FILLET;
/// Goo tiles, pixels: a tile no capsule, fillet or body can reach is cleared without shading.
const TILE: usize = 8;
/// The largest scale the goo is shaded at; above it GPUI scales the image up.
pub const MAX_SCALE: f32 = 1.0;

/// A rectangle in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub origin: Vec2,
    pub size: Vec2,
}

/// A tapered capsule: from, to, and its radius at each end.
type Segment = (Vec2, Vec2, f32, f32);

/// The goo is shaded as separate pieces, each into its own image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Piece {
    /// The attached neck, from the dock to the body.
    Neck,
    /// After the break, the dock's part: it hangs from the line and retracts into it.
    Drip,
    /// After the break, the cloud's part: it keeps its shape, travels with the body and melts
    /// into it.
    Stub,
}

/// A quadratic curve out of the dock: its control point and its end.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Curve {
    ctrl: Vec2,
    end: Vec2,
}

/// Where the neck is in its life.
#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Attached,
    /// Broken `since` seconds ago. The curve is frozen as it was at the break, its end pulled
    /// back to [`SNAP_LEN`], so no later move can stretch either piece.
    Recoiling {
        since: f32,
        curve: Curve,
    },
    /// The recoil is over: nothing of the neck is left to draw.
    Gone,
}

/// One pull, from the dock line to wherever the body is dragged.
#[derive(Clone, Debug, PartialEq)]
pub struct Neck {
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
    /// Reduced, the neck still breaks, but no goo is drawn and the body stays round.
    motion: Motion,
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
            motion: Motion::Full,
        }
    }

    /// Follows the operating system's reduced-motion setting. Switched on mid-recoil, the
    /// recoil ends at once.
    pub(crate) fn set_motion(&mut self, motion: Motion) {
        self.motion = motion;
        if motion == Motion::Reduced && matches!(self.state, State::Recoiling { .. }) {
            self.state = State::Gone;
        }
    }

    pub fn broken(&self) -> bool {
        self.state != State::Attached
    }

    /// The point on the body the neck holds: just under the crown.
    fn attach(&self) -> Vec2 {
        self.body + v2(0.0, -self.body_size * 0.12)
    }

    /// Moves the body to `body` after `dt` seconds, steps the spring and the recoil, and breaks
    /// the neck once it's stretched past [`SNAP_LEN`]. A negative or non-finite step counts as
    /// none, and a long one as [`MAX_STEP`].
    pub(crate) fn update(&mut self, dt: f32, body: Vec2) {
        let dt = if dt.is_finite() {
            dt.clamp(0.0, MAX_STEP)
        } else {
            0.0
        };
        self.body = body;
        let (a, b) = (self.dock, self.attach());
        match self.state {
            State::Attached => {
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
                let len = (b - a).len();
                if len > SNAP_LEN {
                    // However far this step jumped, the frozen end is on the snap circle.
                    let curve = Curve {
                        ctrl: self.ctrl,
                        end: a + scaled(b - a, SNAP_LEN / len),
                    };
                    self.state = match self.motion {
                        Motion::Full => State::Recoiling { since: 0.0, curve },
                        Motion::Reduced => State::Gone,
                    };
                }
            }
            State::Recoiling { since, curve } => {
                let since = since + dt;
                self.state = if since > RECOIL_SECS {
                    State::Gone
                } else {
                    State::Recoiling { since, curve }
                };
            }
            State::Gone => {}
        }
    }

    /// The live curve while attached, the frozen one after the break.
    fn curve(&self) -> Curve {
        match self.state {
            State::Recoiling { curve, .. } => curve,
            State::Attached | State::Gone => Curve {
                ctrl: self.ctrl,
                end: self.attach(),
            },
        }
    }

    /// How far `k` is stretched toward breaking, 0..=1.
    fn stretch(&self, k: Curve) -> f32 {
        ((k.end - self.dock).len() / SNAP_LEN).clamp(0.0, 1.0)
    }

    /// Point `u` (0 at the dock, 1 at the cloud) on curve `k`.
    fn along(&self, k: Curve, u: f32) -> Vec2 {
        scaled(self.dock, (1.0 - u) * (1.0 - u))
            + scaled(k.ctrl, 2.0 * u * (1.0 - u))
            + scaled(k.end, u * u)
    }

    /// The neck's radius at `u` on `k`: a visible stem under the line that thins as it
    /// stretches, swelling smoothly into the cloud.
    fn radius(&self, k: Curve, u: f32) -> f32 {
        let width = self.body_size * 0.73;
        let root = width * mix(0.15, 0.1, self.stretch(k));
        let len = (k.end - self.dock).len().max(1.0);
        let flare = 1.0 - (self.body_size * 0.45 / len).min(0.45);
        mix(root, width * 0.3, smoothstep(flare, 1.0, u))
    }

    /// How the dragged body is scaled about its bottom centre: drawn out toward the dock while
    /// attached, springing back to round after the break.
    pub fn body_scale(&self) -> Vec2 {
        let pull = match (self.motion, self.state) {
            (Motion::Reduced, _) | (_, State::Gone) => 0.0,
            (Motion::Full, State::Attached) => self.stretch(self.curve()),
            // It broke at full stretch.
            (Motion::Full, State::Recoiling { since, .. }) => {
                (-since * 9.0).exp() * (since * 26.0).cos()
            }
        };
        v2(1.0 - 0.07 * pull, 1.0 + 0.14 * pull)
    }

    /// A piece's capsules and its root fillet's radius (0 for none), or `None` when that piece
    /// has nothing to draw now.
    fn piece(&self, piece: Piece) -> Option<([Segment; NECK], f32)> {
        if self.motion == Motion::Reduced {
            return None;
        }
        let k = self.curve();
        let fill = |from: f32, to: f32, shift: Vec2, r: &dyn Fn(f32) -> f32| {
            let mut segs = [(self.dock, self.dock, 0.0, 0.0); NECK];
            for (i, seg) in segs.iter_mut().enumerate() {
                let (u0, u1) = (mix(from, to, fraction(i)), mix(from, to, fraction(i + 1)));
                *seg = (
                    self.along(k, u0) + shift,
                    self.along(k, u1) + shift,
                    r(u0),
                    r(u1),
                );
            }
            segs
        };
        let none = v2(0.0, 0.0);
        match (piece, self.state) {
            (Piece::Neck, State::Attached) => Some((
                fill(0.0, 1.0, none, &|u| self.radius(k, u)),
                self.radius(k, 0.0) * 0.9,
            )),
            (Piece::Drip, State::Recoiling { since, .. }) => {
                let e = smoothstep(0.0, 1.0, since / RECOIL_SECS);
                let drip = BREAK_AT * (1.0 - e * e);
                if drip <= 0.01 {
                    return None;
                }
                // It ends in a bulb, and its root fillet shrinks with it, so the root never turns
                // into a blunt tube.
                let root = self.radius(k, 0.0);
                let bulb = root * 1.1 * (1.0 - e);
                let r = |u| {
                    self.radius(k, u)
                        .max(bulb * smoothstep(drip * 0.6, drip, u))
                };
                Some((fill(0.0, drip, none, &r), root * 0.9 * (1.0 - e)))
            }
            (Piece::Stub, State::Recoiling { since, .. }) => {
                let x = (since / STUB_SECS).clamp(0.0, 1.0);
                let m = 1.0 - (1.0 - x) * (1.0 - x) * (1.0 - x);
                if m >= 1.0 {
                    return None;
                }
                // Its broken end rounds off as it melts up into the cloud.
                let from = mix(STUB_FROM, 0.97, m);
                let r = |u| {
                    self.radius(k, u) * mix(0.55, 1.0, smoothstep(from, mix(from, 1.0, 0.4), u))
                };
                Some((fill(from, 1.0, self.attach() - k.end, &r), 0.0))
            }
            _ => None,
        }
    }

    /// The region `piece`'s image must cover, or `None` when there's nothing to draw.
    pub fn bounds(&self, piece: Piece) -> Option<Rect> {
        let (segs, root) = self.piece(piece)?;
        let pad = Self::pad(piece, self.body_size);
        let ends = segs
            .iter()
            .flat_map(|&(a, b, ra, rb)| [(a, v2(ra + pad, ra + pad)), (b, v2(rb + pad, rb + pad))]);
        // The root fillet is an ellipse 2.2 times as wide as it is tall, centred on the dock.
        let fillet = (root > 0.0).then_some((self.dock, v2(root * 2.2 + pad, root + pad)));
        let (mut lo, mut hi) = (v2(f32::MAX, f32::MAX), v2(f32::MIN, f32::MIN));
        for (p, r) in ends.chain(fillet).chain(self.root_join(piece, root, pad)) {
            lo = v2(lo.x.min(p.x - r.x), lo.y.min(p.y - r.y));
            hi = v2(hi.x.max(p.x + r.x), hi.y.max(p.y + r.y));
        }
        Some(Rect {
            origin: v2(lo.x.floor(), lo.y.floor()),
            size: v2((hi.x - lo.x).ceil() + 1.0, (hi.y - lo.y).ceil() + 1.0),
        })
    }

    /// The box, as a centre and half-size, holding the body's join around the root, or `None`
    /// when there's none: the root's ellipse grown by `pad` in its own 2.2× wide metric. The
    /// shader joins only within [`BODY_REACH`] of the body's centre, so that disc cuts it, at its
    /// widest over the rows the two share.
    fn root_join(&self, piece: Piece, root: f32, pad: f32) -> Option<(Vec2, Vec2)> {
        if piece == Piece::Drip || root <= 0.0 {
            return None;
        }
        let r = root + pad;
        let disc = self.body_size * BODY_REACH;
        let (top, bottom) = (
            (self.dock.y - r).max(self.body.y - disc),
            (self.dock.y + r).min(self.body.y + disc),
        );
        if top > bottom {
            return None;
        }
        let dy = self.body.y - self.body.y.clamp(top, bottom);
        let half = (disc * disc - dy * dy).max(0.0).sqrt();
        let (left, right) = (
            (self.dock.x - r * 2.2).max(self.body.x - half),
            (self.dock.x + r * 2.2).min(self.body.x + half),
        );
        (left <= right).then(|| {
            (
                v2(f32::midpoint(left, right), f32::midpoint(top, bottom)),
                v2((right - left) / 2.0, (bottom - top) / 2.0),
            )
        })
    }

    /// The margin a region keeps around its capsules, and the neck's around its root. The drip
    /// never joins the body and pads by a tenth of it. The shader joins wherever the neck's field
    /// is within 1.5 fillets, and that field is the capsules alone for the stub. For the neck it's
    /// their smooth union with the root, which is never more than a quarter of the blend below
    /// the nearer of the two, so the neck pads both by that much more.
    fn pad(piece: Piece, body_size: f32) -> f32 {
        match piece {
            Piece::Drip => body_size * 0.1,
            Piece::Stub => body_size * FILLET * 1.5,
            Piece::Neck => body_size * (FILLET * 1.5 + ROOT_BLEND / 4.0),
        }
    }

    /// The widest `piece`'s region can get for a body this size, logical pixels. Each piece of
    /// the curve lies in the hull of its own control points, all within [`SNAP_LEN`] of the dock
    /// for the neck, within the drip's reach of the dock for the drip, and within
    /// `(1 − STUB_FROM²) × 2 × SNAP_LEN` of the attachment for the stub. Its widest capsule,
    /// fillet and pad go around that, and two pixels of rounding. The root's join reaches at most
    /// `2.2 × (0.9 × 0.73 × 0.15 + 1.5 × FILLET + ROOT_BLEND / 4)`, 0.54 of the body, from the
    /// dock, well inside the neck's cap.
    fn cap(piece: Piece, body_size: f32) -> f32 {
        let widest = body_size * 0.73 * 0.3;
        let root = body_size * 0.73 * 0.15;
        let pad = Self::pad(piece, body_size);
        let half = match piece {
            Piece::Neck => SNAP_LEN + widest + pad,
            Piece::Drip => {
                (2.0 * BREAK_AT - BREAK_AT * BREAK_AT) * SNAP_LEN
                    + root * 1.1
                    + root * 0.9 * 2.2
                    + pad
            }
            Piece::Stub => (1.0 - STUB_FROM * STUB_FROM) * 2.0 * SNAP_LEN + widest + pad,
        };
        2.0 * half + 2.0
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
pub struct GooMismatch {
    width: usize,
    height: usize,
    got: usize,
    max: usize,
}

/// Shades pieces of the goo. Its scratch is sized once, from the largest region its pieces can
/// need for a body this size at [`MAX_SCALE`], so shading a frame allocates nothing.
#[derive(Debug)]
pub struct GooShader {
    max: usize,
    tiles: Vec<(bool, u16)>,
    above: Vec<f32>,
    row: Vec<f32>,
}

impl GooShader {
    /// Scratch for the attached neck and the drip, which never coexist.
    pub fn neck(body_size: f32) -> Self {
        Self::sized(Neck::cap(Piece::Neck, body_size).max(Neck::cap(Piece::Drip, body_size)))
    }

    /// Scratch for the stub.
    pub fn stub(body_size: f32) -> Self {
        Self::sized(Neck::cap(Piece::Stub, body_size))
    }

    fn sized(cap: f32) -> Self {
        let max = pixels(cap, MAX_SCALE);
        let tiles = max.div_ceil(TILE);
        Self {
            max,
            tiles: vec![(false, 0); tiles * tiles],
            above: vec![f32::MAX; max],
            row: vec![f32::MAX; max],
        }
    }

    /// The largest image side, pixels, this shader can shade.
    pub fn capacity(&self) -> usize {
        self.max
    }

    /// The image size, pixels, of region `r` at `scale`.
    pub fn size(r: Rect, scale: f32) -> (usize, usize) {
        (pixels(r.size.x, scale), pixels(r.size.y, scale))
    }

    /// Shades `piece` for region `r` at `scale` pixels per logical pixel into `out`,
    /// straight-alpha BGRA of [`Self::size`], with the body's look and frame so its colours
    /// carry on into the goo. Clear pixels take the body's top colour, never black.
    ///
    /// # Errors
    /// [`GooMismatch`] when `out` isn't the region's size, or the region or scale is larger
    /// than this shader's scratch.
    #[expect(
        clippy::too_many_arguments,
        reason = "the neck and piece, its region, the pixels and the body's shape, look and frame"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "one pass over the pixels, ported whole from the approved spike"
    )]
    pub fn render(
        &mut self,
        neck: &Neck,
        piece: Piece,
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
        let Some((segs, root_r)) = neck.piece(piece) else {
            out.as_chunks_mut::<4>().0.fill(clear);
            return Ok(());
        };
        let s = neck.body_size;
        // The drip hangs from the line, apart from the body; the neck and the stub melt into it.
        let joins = piece != Piece::Drip;
        let framing = Framing::CARD;
        let units_per_px = framing.span / s;
        let fillet = s * FILLET;
        // Goo glows less than the body it came from.
        let f = &FrameState {
            glow_k: f.glow_k * 0.8,
            ..*f
        };
        let body_reach = s * BODY_REACH;
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
            for (k, &(a, b, ra, rb)) in segs.iter().enumerate() {
                if tapered(c, a, b, ra, rb) < seg_reach {
                    mask |= 1 << k;
                }
            }
            let busy = mask != 0
                || (joins && (c - neck.body).len() < body_reach + reach)
                || (root_r > 0.0 && (c - neck.dock).len() - root_r * 2.2 < reach);
            *slot = (busy, mask);
        }
        let (above, row) = (&mut self.above[..w], &mut self.row[..w]);
        row.fill(f32::MAX);
        for j in 0..h {
            above.copy_from_slice(row);
            row.fill(f32::MAX);
            for i in 0..w {
                let (busy, mask) = self.tiles[(j / TILE) * tw + i / TILE];
                let o = &mut out[(j * w + i) * 4..][..4];
                if !busy {
                    o.copy_from_slice(&clear);
                    continue;
                }
                let p = r.origin + v2((index_f32(i) + 0.5) * px, (index_f32(j) + 0.5) * px);
                let q = to_body(p);
                let near_body = joins && (p - neck.body).len() < body_reach;
                let neck_at = |p: Vec2| {
                    let mut d = f32::MAX;
                    for (k, &(a, b, ra, rb)) in segs.iter().enumerate() {
                        if mask & (1 << k) != 0 {
                            d = d.min(tapered(p, a, b, ra, rb));
                        }
                    }
                    if root_r <= 0.0 {
                        return d;
                    }
                    // A soft fillet where the neck leaves the line, so it grows out of the dock.
                    let rel = p - neck.dock;
                    smin(d, v2(rel.x / 2.2, rel.y).len() - root_r, s * ROOT_BLEND)
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
    use super::{
        Curve, GooMismatch, GooShader, MAX_SCALE, Neck, Piece, Rect, SNAP_LEN, State, pixels,
        tapered,
    };
    use crate::presence::kumo::expression::Motion;
    use crate::presence::kumo::frame::FrameState;
    use crate::presence::kumo::look::Look;
    use crate::presence::kumo::math::{Vec2, v2};
    use crate::presence::kumo::shape::Silhouette;

    const FRAME: f32 = 1.0 / 72.0;
    const BODY: f32 = 84.0;
    const DOCK: Vec2 = v2(400.0, 10.0);

    /// Where the body is when its attachment is `len` straight below the dock.
    fn below(len: f32) -> Vec2 {
        v2(DOCK.x, DOCK.y + len + BODY * 0.12)
    }

    /// A neck pulled straight down until its attachment is `len` below the dock.
    fn pulled(len: f32) -> Neck {
        let mut neck = Neck::new(DOCK, BODY);
        for _ in 0..72 {
            neck.update(FRAME, below(len));
        }
        neck
    }

    /// A neck pulled to 140, then broken by one step to `len`.
    fn snapped(len: f32) -> Neck {
        let mut neck = pulled(140.0);
        neck.update(FRAME, below(len));
        neck
    }

    fn shader_for(piece: Piece) -> GooShader {
        match piece {
            Piece::Neck | Piece::Drip => GooShader::neck(BODY),
            Piece::Stub => GooShader::stub(BODY),
        }
    }

    fn shade(neck: &Neck, piece: Piece, shader: &mut GooShader) -> (usize, usize, Vec<u8>) {
        let r = neck.bounds(piece).expect("the piece has goo");
        shade_in(neck, piece, r, MAX_SCALE, shader)
    }

    fn shade_in(
        neck: &Neck,
        piece: Piece,
        r: Rect,
        scale: f32,
        shader: &mut GooShader,
    ) -> (usize, usize, Vec<u8>) {
        let (w, h) = GooShader::size(r, scale);
        let mut out = vec![0; w * h * 4];
        let f = FrameState {
            body_scale: neck.body_scale(),
            ..FrameState::IDLE
        };
        shader
            .render(
                neck,
                piece,
                r,
                scale,
                &Silhouette::emoji(),
                &Look::default(),
                &f,
                &mut out,
            )
            .expect("the region fits the shader");
        (w, h, out)
    }

    /// Whether `r` lies inside `piece`'s cap, a square centred on `centre`.
    fn inside_cap(r: Rect, piece: Piece, centre: Vec2) -> bool {
        let half = Neck::cap(piece, BODY) / 2.0;
        r.origin.x >= centre.x - half
            && r.origin.y >= centre.y - half
            && r.origin.x + r.size.x <= centre.x + half
            && r.origin.y + r.size.y <= centre.y + half
    }

    fn close(a: Vec2, b: Vec2) -> bool {
        (a - b).len() < 1e-3
    }

    #[test]
    fn the_neck_breaks_one_unit_past_the_snap_length() {
        assert_eq!(pulled(0.0).body_scale(), v2(1.0, 1.0), "slack, it is round");
        let mut neck = pulled(SNAP_LEN);
        let stretched = neck.body_scale();
        assert!(
            close(stretched, v2(0.93, 1.14)),
            "the body reaches full stretch: {stretched:?}"
        );
        assert!(!neck.broken(), "attached at exactly {SNAP_LEN}");
        assert!(neck.bounds(Piece::Neck).is_some());
        assert_eq!(neck.bounds(Piece::Drip), None);
        neck.update(FRAME, below(SNAP_LEN + 1.0));
        assert!(neck.broken(), "broken one unit past");
        assert_eq!(neck.bounds(Piece::Neck), None, "the neck itself is gone");
        assert!(neck.bounds(Piece::Drip).is_some() && neck.bounds(Piece::Stub).is_some());
        assert_eq!(neck.body_scale(), stretched, "it broke at full stretch");
    }

    #[test]
    fn a_break_recoils_and_then_nothing_is_left() {
        let mut neck = snapped(150.0);
        let body = below(150.0);
        let pieces = |n: &Neck| {
            (
                n.bounds(Piece::Drip).is_some(),
                n.bounds(Piece::Stub).is_some(),
            )
        };
        assert_eq!(pieces(&neck), (true, true));
        neck.update(0.1, body);
        assert_eq!(
            pieces(&neck),
            (true, true),
            "0.1 s: the stub is still melting"
        );
        // exp(-0.9) cos(2.6) = -0.3484: the body overshoots round.
        assert!(
            close(neck.body_scale(), v2(1.024_39, 0.951_22)),
            "{:?}",
            neck.body_scale()
        );
        neck.update(0.1, body);
        assert_eq!(
            pieces(&neck),
            (true, false),
            "0.2 s: past the stub's 0.12 s"
        );
        neck.update(0.1, body);
        assert_eq!(
            pieces(&neck),
            (false, false),
            "0.3 s: the drip is back in the line"
        );
        assert!(matches!(neck.state, State::Recoiling { .. }));
        neck.update(0.1, body);
        assert_eq!(neck.state, State::Gone, "past the 0.32 s recoil");
        assert_eq!(neck.body_scale(), v2(1.0, 1.0), "the body is round again");
        assert!(neck.broken());
    }

    #[test]
    fn an_invalid_step_counts_as_none_and_a_long_one_as_a_tenth_of_a_second() {
        let start = pulled(100.0);
        let body = v2(DOCK.x + 60.0, DOCK.y + 100.0);
        let region = Rect {
            origin: v2(300.0, -50.0),
            size: v2(240.0, 240.0),
        };
        let after = |dt: f32| {
            let mut neck = start.clone();
            neck.update(dt, body);
            shade_in(&neck, Piece::Neck, region, 0.5, &mut GooShader::neck(BODY)).2
        };
        let zero = after(0.0);
        let clamped = after(0.1);
        for dt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0] {
            assert_eq!(after(dt), zero, "{dt}");
        }
        assert_eq!(after(1.0e9), clamped);
        let mask = |pixels: &[u8]| {
            pixels
                .iter()
                .skip(3)
                .step_by(4)
                .map(|&alpha| alpha != 0)
                .collect::<Vec<_>>()
        };
        let (zero, clamped) = (mask(&zero), mask(&clamped));
        assert!(zero.contains(&true) && clamped.contains(&true));
        assert_ne!(zero, clamped, "the spring visibly moved");
    }

    #[test]
    fn the_region_stays_within_the_cap_through_a_fast_swing() {
        let mut neck = Neck::new(DOCK, BODY);
        // Whip the body side to side well inside the snap length.
        for k in 0..288_u16 {
            let side = if (k / 6) % 2 == 0 { 1.0 } else { -1.0 };
            neck.update(FRAME, DOCK + v2(side * 90.0, 100.0));
            let r = neck.bounds(Piece::Neck).expect("still attached");
            assert!(inside_cap(r, Piece::Neck, DOCK), "frame {k}: {r:?}");
        }
    }

    /// Every pixel with alpha that a 40 px wider image of `piece` on the same grid holds outside
    /// its region, as logical centres.
    fn clipped(neck: &Neck, piece: Piece, scale: f32) -> Vec<(f32, f32, u8)> {
        let Some(r) = neck.bounds(piece) else {
            return Vec::new();
        };
        let grow = 40.0;
        let wide = Rect {
            origin: r.origin - v2(grow, grow),
            size: r.size + v2(2.0 * grow, 2.0 * grow),
        };
        let (w, h) = GooShader::size(r, scale);
        let mut shader = GooShader::sized(Neck::cap(piece, BODY) + 2.0 * grow);
        let (ww, wh, out) = shade_in(neck, piece, wide, scale, &mut shader);
        let skip = pixels(grow, scale);
        let centre = |i: usize| (super::index_f32(i) + 0.5) / scale;
        (0..wh)
            .flat_map(|j| (0..ww).map(move |i| (i, j)))
            .filter(|&(i, j)| !((skip..skip + w).contains(&i) && (skip..skip + h).contains(&j)))
            .map(|(i, j)| {
                let a = out[(j * ww + i) * 4 + 3];
                (wide.origin.x + centre(i), wide.origin.y + centre(j), a)
            })
            .filter(|&(.., a)| a > 0)
            .collect()
    }

    #[test]
    fn no_piece_draws_outside_its_region() {
        let mut states = Vec::new();
        for body in [v2(400.0, 170.08), v2(400.0, 20.08), v2(420.0, 20.08)] {
            let mut neck = Neck::new(DOCK, BODY);
            neck.update(0.0, body);
            let piece = if neck.broken() {
                Piece::Stub
            } else {
                Piece::Neck
            };
            assert!(neck.bounds(piece).is_some(), "{body:?} draws a first frame");
            states.push(neck);
        }
        for len in [0.0, 0.08, 2.0, 6.0, 15.0, 30.0, 55.0, 80.0, 110.0, 143.9] {
            for dx in [-70.0, -15.0, 0.0, 5.0, 20.0, 60.0] {
                let mut neck = Neck::new(DOCK, BODY);
                for _ in 0..72 {
                    neck.update(FRAME, below(len) + v2(dx, 0.0));
                }
                states.push(neck);
            }
        }
        for len in [144.5, 170.0, 400.0] {
            let mut neck = snapped(len);
            states.push(neck.clone());
            for frames in [3, 8, 14, 22] {
                for _ in 0..frames {
                    neck.update(FRAME, below(len) + v2(30.0, 0.0));
                }
                states.push(neck.clone());
            }
        }
        let mut bad = Vec::new();
        for (k, neck) in states.iter().enumerate() {
            for piece in [Piece::Neck, Piece::Drip, Piece::Stub] {
                for scale in [0.5, MAX_SCALE] {
                    let c = clipped(neck, piece, scale);
                    if !c.is_empty() {
                        bad.push((k, piece, scale, c.into_iter().take(4).collect::<Vec<_>>()));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "{bad:?}");
    }

    #[test]
    fn a_jump_on_the_crossing_step_freezes_the_end_on_the_snap_circle() {
        let neck = snapped(3000.0);
        let State::Recoiling { curve, .. } = neck.state else {
            panic!("broken: {:?}", neck.state);
        };
        assert!(close(curve.end, DOCK + v2(0.0, SNAP_LEN)), "{curve:?}");
        assert!(inside_cap(
            neck.bounds(Piece::Drip).expect("a drip"),
            Piece::Drip,
            DOCK
        ));
        let stub = neck.bounds(Piece::Stub).expect("a stub");
        assert!(inside_cap(stub, Piece::Stub, neck.attach()), "{stub:?}");
    }

    #[test]
    fn extremal_controls_keep_both_pieces_inside_their_caps() {
        for (ctrl, end) in [
            (v2(144.0, 0.0), v2(0.0, 144.0)),
            (v2(0.0, 144.0), v2(0.0, 144.0)),
        ] {
            let mut neck = Neck::new(v2(0.0, 0.0), BODY);
            neck.body = end + v2(0.0, BODY * 0.12);
            let curve = Curve { ctrl, end };
            neck.state = State::Recoiling { since: 0.0, curve };
            for (piece, cap, center) in [(Piece::Drip, 352, v2(0.0, 0.0)), (Piece::Stub, 613, end)]
            {
                let region = neck.bounds(piece).expect("the split piece is drawable");
                assert!(inside_cap(region, piece, center), "{piece:?} at {region:?}");
                let (width, height, out) =
                    shade_in(&neck, piece, region, MAX_SCALE, &mut shader_for(piece));
                assert!(
                    (1..=cap).contains(&width) && (1..=cap).contains(&height),
                    "{curve:?} {piece:?}"
                );
                assert!(out.iter().skip(3).step_by(4).any(|&alpha| alpha > 0));
            }
        }
        let mut neck = Neck::new(v2(0.0, 0.0), BODY);
        neck.body = v2(0.0, 144.0 + BODY * 0.12);
        neck.state = State::Recoiling {
            since: 0.0,
            curve: Curve {
                ctrl: v2(0.0, 144.0),
                end: v2(0.0, 144.0),
            },
        };
        let sample = |y: f32| {
            let region = Rect {
                origin: v2(-0.5, y - 0.5),
                size: v2(1.0, 1.0),
            };
            shade_in(
                &neck,
                Piece::Drip,
                region,
                MAX_SCALE,
                &mut GooShader::neck(BODY),
            )
            .2[3]
        };
        assert_eq!(sample(42.0), 255, "the drip reaches the break");
        assert_eq!(
            sample(55.0),
            0,
            "the drip ends near the dock, not in the stub"
        );
    }

    #[test]
    fn a_far_move_after_the_break_leaves_the_drip_and_carries_the_stub() {
        let (mut moved, mut still) = (snapped(150.0), snapped(150.0));
        let stub = |n: &Neck| {
            let (segs, _) = n.piece(Piece::Stub).expect("a stub");
            segs.map(|(a, b, ra, rb)| (a - n.attach(), b - n.attach(), ra, rb))
        };
        for (frames, far) in [(1, v2(3400.0, 1500.0)), (3, v2(-2600.0, 3010.0))] {
            for _ in 0..frames {
                moved.update(FRAME, far);
                still.update(FRAME, below(150.0));
            }
            assert_eq!(
                moved.bounds(Piece::Drip),
                still.bounds(Piece::Drip),
                "the drip stays"
            );
            for (m, s) in stub(&moved).iter().zip(stub(&still)) {
                let radii = v2(m.2 - s.2, m.3 - s.3).len();
                assert!(
                    close(m.0, s.0) && close(m.1, s.1) && radii < 1e-3,
                    "{m:?} {s:?}"
                );
            }
            let r = moved.bounds(Piece::Stub).expect("a stub");
            assert!(inside_cap(r, Piece::Stub, moved.attach()), "{r:?}");
        }
    }

    #[test]
    fn reduced_motion_breaks_without_any_goo() {
        let mut neck = pulled(100.0);
        neck.set_motion(Motion::Reduced);
        assert_eq!(neck.bounds(Piece::Neck), None);
        assert_eq!(neck.body_scale(), v2(1.0, 1.0));
        neck.update(FRAME, below(150.0));
        assert!(neck.broken(), "the snap is still decided");
        assert_eq!(neck.state, State::Gone, "with no recoil");
        let mut recoiling = snapped(150.0);
        recoiling.set_motion(Motion::Reduced);
        assert_eq!(recoiling.state, State::Gone, "switched on mid-recoil");
        assert_eq!(recoiling.bounds(Piece::Drip), None);
    }

    #[test]
    fn no_goo_pixel_is_black_clear_or_covered() {
        for (neck, piece) in [
            (pulled(100.0), Piece::Neck),
            (snapped(150.0), Piece::Drip),
            (snapped(150.0), Piece::Stub),
        ] {
            let (width, height, out) = shade(&neck, piece, &mut shader_for(piece));
            let px = out.as_chunks::<4>().0;
            assert!(
                px.iter().any(|p| p[3] == 255),
                "{piece:?}: some goo is solid"
            );
            assert!(px.iter().any(|p| p[3] == 0), "{piece:?}: some goo is clear");
            if piece == Piece::Neck {
                assert!(height > 100, "the region spans the full straight neck");
                assert_eq!(
                    px[(height / 2) * width + width / 2][3],
                    255,
                    "solid straight midline"
                );
                assert_eq!(
                    (px[0][3], px[width - 1][3]),
                    (0, 0),
                    "transparent top corners"
                );
            }
            let black = px.iter().filter(|p| p[..3] == [0, 0, 0]).count();
            assert_eq!(black, 0, "{piece:?}: a scaled edge blends toward these");
        }
    }

    #[test]
    fn shading_twice_gives_the_same_bytes_and_keeps_the_scratch() {
        let neck = pulled(100.0);
        let drip = snapped(150.0);
        let mut shader = GooShader::neck(BODY);
        let (shape, look) = (Silhouette::emoji(), Look::default());
        let region = neck.bounds(Piece::Neck).expect("a neck");
        let (width, height) = GooShader::size(region, MAX_SCALE);
        let mut out = vec![0; width * height * 4];
        let mut render = |model: &Neck, piece, out: &mut [u8]| {
            shader.render(
                model,
                piece,
                region,
                MAX_SCALE,
                &shape,
                &look,
                &FrameState::IDLE,
                out,
            )
        };
        assert_eq!(render(&neck, Piece::Neck, &mut out), Ok(()));
        let first = out.clone();
        assert_eq!(render(&drip, Piece::Drip, &mut out), Ok(()));
        assert_ne!(out, first, "the intervening frame must change the image");
        let (result, [allocations, _], _) =
            crate::allocation::usage(|| render(&neck, Piece::Neck, &mut out));
        assert_eq!(result, Ok(()));
        assert_eq!(allocations, 0);
        assert_eq!(out, first);
    }

    #[test]
    fn the_scratch_holds_its_cap_and_one_pixel_more_is_an_error() {
        // 84 px: the neck's cap is 2 × (144 + 18.396 + 12.39) + 2 = 351.6, the drip's 160.2, and
        // the stub's 2 × (275.2992 + 18.396 + 11.34) + 2 = 612.1, each rounded up.
        assert_eq!(GooShader::neck(BODY).max, 352);
        assert_eq!(GooShader::stub(BODY).max, 613);
        let neck = pulled(100.0);
        let mut shader = GooShader::neck(BODY);
        let origin = neck.bounds(Piece::Neck).expect("goo").origin;
        let mut render = |w: usize, h: usize, scale: f32, len: usize| {
            let size = v2(super::index_f32(w), super::index_f32(h));
            let r = Rect { origin, size };
            shader.render(
                &neck,
                Piece::Neck,
                r,
                scale,
                &Silhouette::emoji(),
                &Look::default(),
                &FrameState::IDLE,
                &mut vec![0; len],
            )
        };
        let err = |width: usize, height: usize, got: usize| {
            Err(GooMismatch {
                width,
                height,
                got,
                max: 352,
            })
        };
        assert_eq!(render(352, 1, 1.0, 352 * 4), Ok(()));
        assert_eq!(render(1, 352, 1.0, 352 * 4), Ok(()));
        assert_eq!(render(353, 1, 1.0, 353 * 4), err(353, 1, 353 * 4));
        assert_eq!(render(1, 353, 1.0, 353 * 4), err(1, 353, 353 * 4));
        assert_eq!(
            render(8, 4, 1.0, 8 * 4 * 4 - 1),
            err(8, 4, 127),
            "a short buffer"
        );
        assert_eq!(
            render(8, 4, 2.0, 16 * 8 * 4),
            err(16, 8, 512),
            "past the scale"
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
