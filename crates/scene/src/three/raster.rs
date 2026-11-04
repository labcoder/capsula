//! A CPU raster of one view's convex screen polygons against a per-band depth buffer.
//!
//! Positions are snapped to 1/256 texel and covered with exact i64 edge functions under the
//! top-left rule. Opaque polygons depth-test and write; translucent ones, sorted back to front by
//! centroid depth, depth-test against them without writing. Rest frames take four rotated-grid
//! samples per texel; motion frames take one at the texel centre.

use crate::limits::MAX_SOLIDS;
use std::sync::atomic::{AtomicBool, Ordering};

/// Snapped positions are in 1/256 texel.
pub const SUBTEXEL: i32 = 256;
/// Largest internal raster width or height, in texels.
pub const MAX_TEXELS: u32 = 4096;
/// Guard band beyond the view on every side, in texels.
pub const GUARD_TEXELS: u32 = 4096;
/// Largest snapped coordinate magnitude, (4,096 + 4,096) × 256 = 2^21, so every edge-function
/// difference stays under 2^22, each product under 2^44 and the function under 2^45.
pub const COORD_LIMIT: i32 = 1 << 21;
/// Most vertices of one polygon: a triangle, one near-plane clip and four guard clips.
pub const MAX_POLY_VERTICES: usize = 8;
/// Rows held in band scratch at once.
pub const BAND_ROWS: u32 = 32;
/// Samples between cancellation polls inside a polygon, plus at most one row span.
pub const POLL_SAMPLES: u32 = 1 << 16;
/// Depth keys drop the three lowest mantissa bits, so depths within about 1e-6 relative tie.
const DEPTH_MASK: u32 = !0b111;
const MOTION: [[i32; 2]; 1] = [[128, 128]];
/// Rotated-grid offsets from the texel origin: (∓1/8, ∓3/8) and (±3/8, ∓1/8) about its centre.
const REST: [[i32; 2]; 4] = [[96, 32], [224, 96], [32, 160], [160, 224]];
const _: () = assert!(MAX_SOLIDS < 256, "solid ids fit a u8 with 0 for background");

/// One polygon as the frame stage hands it over, before normalisation.
#[derive(Debug, Clone, Copy)]
pub struct Fragment<'a> {
    /// Snapped vertices in 1/256 texel, any winding, at most [`MAX_POLY_VERTICES`].
    pub points: &'a [[i32; 2]],
    /// Reciprocal camera depth as `a·x + b·y + c` over texel coordinates.
    pub plane: [f32; 3],
    /// Shaded straight colour.
    pub rgb: [u8; 3],
    /// Authored opacity in 0..=1; 1 is opaque, 0.5 or more is pickable.
    pub opacity: f32,
    /// One-based solid index; 0 is the background id.
    pub solid: u8,
    /// Centroid camera-space depth, the translucent sort key.
    pub centroid_z: f32,
}

/// A normalised, strictly convex, counter-clockwise screen polygon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenPoly {
    points: [[i32; 2]; MAX_POLY_VERTICES],
    len: u8,
    plane: [f32; 3],
    rgb: [u8; 3],
    alpha: u8,
    solid: u8,
    opaque: bool,
    picks: bool,
}

/// Why a fragment could not join the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PushError {
    /// The scratch or a polygon's vertex capacity is full; nothing was written.
    #[error("scene.Full")]
    Full,
    /// A coordinate, solid index or opacity is outside its bound.
    #[error("scene.Invalid")]
    Invalid,
    /// The depth plane or sort key is not finite and positive.
    #[error("scene.Numeric")]
    Numeric,
}

/// Why a frame was not rasterised; the target may hold a partial frame and must not be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RasterError {
    /// The target or band scratch does not match a 1..=4,096 texel frame.
    #[error("scene.Size")]
    Size,
    /// The caller requested cancellation.
    #[error("scene.Cancelled")]
    Cancelled,
}

/// One frame's polygons and translucent sort keys, reserved once.
pub struct FrameScratch {
    polys: Vec<ScreenPoly>,
    keys: Vec<(u32, u32)>,
    discarded: u32,
}

impl std::fmt::Debug for FrameScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameScratch")
            .field("polys", &self.polys.len())
            .field("discarded", &self.discarded)
            .finish_non_exhaustive()
    }
}

impl FrameScratch {
    /// Reserves room for `polygons` polygons and their keys; pushes never grow it.
    pub fn with_capacity(polygons: usize) -> Self {
        Self {
            polys: Vec::with_capacity(polygons),
            keys: Vec::with_capacity(polygons),
            discarded: 0,
        }
    }

    /// Empties the frame, keeping its capacity.
    pub fn clear(&mut self) {
        self.polys.clear();
        self.keys.clear();
        self.discarded = 0;
    }

    /// Polygons kept.
    pub fn len(&self) -> usize {
        self.polys.len()
    }

    /// Whether no polygon was kept.
    pub fn is_empty(&self) -> bool {
        self.polys.is_empty()
    }

    /// Fragments dropped because their snapped loop collapsed or was not strictly convex.
    pub fn discarded(&self) -> u32 {
        self.discarded
    }

    /// Normalises and keeps one fragment, returning whether it covers anything.
    /// # Errors
    /// [`PushError::Full`] when the scratch or vertex capacity is exhausted, [`PushError::Invalid`]
    /// for an out-of-range coordinate, solid or opacity, and [`PushError::Numeric`] for a depth
    /// plane that is not finite and positive at every vertex or a bad translucent key.
    pub fn push(&mut self, fragment: &Fragment<'_>) -> Result<bool, PushError> {
        if self.polys.len() == self.polys.capacity() || fragment.points.len() > MAX_POLY_VERTICES {
            return Err(PushError::Full);
        }
        let solid_ok = fragment.solid >= 1 && usize::from(fragment.solid) <= MAX_SOLIDS;
        if !solid_ok || !(0.0..=1.0).contains(&fragment.opacity) {
            return Err(PushError::Invalid);
        }
        let mut points = [[0; 2]; MAX_POLY_VERTICES];
        for (slot, point) in points.iter_mut().zip(fragment.points) {
            if !point
                .iter()
                .all(|v| (-COORD_LIMIT..=COORD_LIMIT).contains(v))
            {
                return Err(PushError::Invalid);
            }
            *slot = *point;
        }
        let opaque = fragment.opacity >= 1.0;
        let key_ok = fragment.centroid_z.is_finite() && fragment.centroid_z > 0.0;
        if !fragment.plane.iter().all(|v| v.is_finite()) || (!opaque && !key_ok) {
            return Err(PushError::Numeric);
        }
        if fragment.opacity == 0.0 {
            return Ok(false);
        }
        let Some(len) = normalise(&mut points, fragment.points.len()) else {
            self.discarded += 1;
            return Ok(false);
        };
        if !points[..len]
            .iter()
            .all(|p| depth(fragment.plane, p[0], p[1]).is_some())
        {
            return Err(PushError::Numeric);
        }
        let index = u32::try_from(self.polys.len()).map_err(|_| PushError::Full)?;
        if !opaque {
            self.keys.push((fragment.centroid_z.to_bits(), index));
        }
        self.polys.push(ScreenPoly {
            points,
            len: u8::try_from(len).map_err(|_| PushError::Full)?,
            plane: fragment.plane,
            rgb: fragment.rgb,
            alpha: alpha(fragment.opacity),
            solid: fragment.solid,
            opaque,
            picks: fragment.opacity >= 0.5,
        });
        Ok(true)
    }
}

/// Removes duplicate and collinear vertices, makes the loop counter-clockwise in y-down texel
/// space, and returns its length only if it is strictly convex with positive area.
fn normalise(points: &mut [[i32; 2]; MAX_POLY_VERTICES], mut n: usize) -> Option<usize> {
    let mut i = 0;
    while n >= 3 && i < n {
        if cross(points[(i + n - 1) % n], points[i], points[(i + 1) % n]) == 0 {
            points.copy_within(i + 1..n, i);
            n -= 1;
            i = i.saturating_sub(1);
        } else {
            i += 1;
        }
    }
    if n < 3 {
        return None;
    }
    let area: i64 = (0..n).map(|i| wedge(points[i], points[(i + 1) % n])).sum();
    if area < 0 {
        points[..n].reverse();
    }
    (0..n)
        .all(|i| cross(points[(i + n - 1) % n], points[i], points[(i + 1) % n]) > 0)
        .then_some(n)
}

fn wedge(a: [i32; 2], b: [i32; 2]) -> i64 {
    i64::from(a[0]) * i64::from(b[1]) - i64::from(b[0]) * i64::from(a[1])
}

fn cross(a: [i32; 2], b: [i32; 2], c: [i32; 2]) -> i64 {
    let [abx, aby] = [
        i64::from(b[0]) - i64::from(a[0]),
        i64::from(b[1]) - i64::from(a[1]),
    ];
    let [bcx, bcy] = [
        i64::from(c[0]) - i64::from(b[0]),
        i64::from(c[1]) - i64::from(b[1]),
    ];
    abx * bcy - aby * bcx
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "opacity is checked to 0..=1, so the rounded product is 0..=255"
)]
fn alpha(opacity: f32) -> u8 {
    (opacity * 255.0).round() as u8
}

/// The masked depth key at a sample in 1/256 texel, or none when not finite and positive.
fn depth(plane: [f32; 3], x: i32, y: i32) -> Option<u32> {
    let [gx, gy, base] = plane.map(f64::from);
    let unit = f64::from(SUBTEXEL);
    Some(key(
        gx * f64::from(x) / unit + (gy * f64::from(y) / unit + base)
    ))
    .filter(|&k| k != 0)
}

/// Narrows a depth to its masked key; 0, which never wins, when not finite and positive.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "depth narrows to f32 deliberately and is checked at once"
)]
fn key(value: f64) -> u32 {
    let value = value as f32;
    if value.is_finite() && value > 0.0 {
        value.to_bits() & DEPTH_MASK
    } else {
        0
    }
}

/// Samples per texel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// One sample at the texel centre.
    Motion,
    /// Four rotated-grid samples, box-resolved.
    Rest,
}

/// Per-band depth, premultiplied colour and id samples for frames up to one width.
pub struct BandScratch {
    width: u32,
    depth: Box<[u32]>,
    color: Box<[u32]>,
    ids: Box<[u8]>,
}

impl std::fmt::Debug for BandScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BandScratch")
            .field("width", &self.width)
            .finish_non_exhaustive()
    }
}

impl BandScratch {
    /// Reserves 32 rows × `width` × 4 samples once: 1,152 bytes per column.
    /// # Errors
    /// [`RasterError::Size`] unless `width` is 1..=4,096.
    pub fn new(width: u32) -> Result<Self, RasterError> {
        if !(1..=MAX_TEXELS).contains(&width) {
            return Err(RasterError::Size);
        }
        let samples = us(BAND_ROWS * width * 4);
        Ok(Self {
            width,
            depth: vec![0; samples].into_boxed_slice(),
            color: vec![0; samples].into_boxed_slice(),
            ids: vec![0; samples].into_boxed_slice(),
        })
    }
}

/// The frame being built: straight-alpha BGRA texels and their solid ids, row-major.
pub struct Target<'a> {
    /// Texels per row.
    pub width: u32,
    /// Rows.
    pub height: u32,
    /// `width × height` texels; each u32's little-endian bytes are B, G, R, A.
    pub color: &'a mut [u32],
    /// `width × height` solid ids; 0 is background.
    pub ids: &'a mut [u8],
}

impl std::fmt::Debug for Target<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Rasterises one frame: opaque polygons in input order, then translucent ones back to front by
/// centroid depth, then the resolve and a clear-texel colour bleed. Allocates nothing.
/// # Errors
/// [`RasterError::Size`] for a target outside 1..=4,096 texels, mismatched buffers or a narrower
/// band, and [`RasterError::Cancelled`] when `cancel` is set; it is polled per polygon, after at
/// most [`POLL_SAMPLES`] samples plus one row span, and per resolved and bled row.
pub fn raster(
    frame: &mut FrameScratch,
    quality: Quality,
    band: &mut BandScratch,
    target: Target<'_>,
    cancel: &AtomicBool,
) -> Result<(), RasterError> {
    let Target {
        width,
        height,
        color,
        ids,
    } = target;
    let texels = us(width) * us(height);
    if !(1..=MAX_TEXELS).contains(&width)
        || !(1..=MAX_TEXELS).contains(&height)
        || width > band.width
        || color.len() != texels
        || ids.len() != texels
    {
        return Err(RasterError::Size);
    }
    let offsets: &[[i32; 2]] = match quality {
        Quality::Motion => &MOTION,
        Quality::Rest => &REST,
    };
    frame
        .keys
        .sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut work = Work {
        cancel,
        left: POLL_SAMPLES,
    };
    work.poll()?;
    let mut y0 = 0;
    while y0 < height {
        let rows = BAND_ROWS.min(height - y0);
        let band_samples = us(rows * width) * offsets.len();
        band.depth[..band_samples].fill(0);
        band.color[..band_samples].fill(0);
        band.ids[..band_samples].fill(0);
        let region = Region {
            width,
            y0,
            rows,
            offsets,
        };
        for poly in frame.polys.iter().filter(|p| p.opaque) {
            fill(poly, &region, band, &mut work)?;
        }
        for &(_, index) in &frame.keys {
            fill(&frame.polys[us(index)], &region, band, &mut work)?;
        }
        resolve(&region, band, color, ids, &mut work)?;
        y0 += rows;
    }
    bleed(width, height, color, &mut work)
}

struct Region<'a> {
    width: u32,
    y0: u32,
    rows: u32,
    offsets: &'a [[i32; 2]],
}

struct Work<'a> {
    cancel: &'a AtomicBool,
    left: u32,
}

impl Work<'_> {
    fn poll(&mut self) -> Result<(), RasterError> {
        self.left = POLL_SAMPLES;
        if self.cancel.load(Ordering::Relaxed) {
            Err(RasterError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn spend(&mut self, samples: u32) -> Result<(), RasterError> {
        if samples >= self.left {
            return self.poll();
        }
        self.left -= samples;
        Ok(())
    }
}

fn fill(
    poly: &ScreenPoly,
    region: &Region<'_>,
    band: &mut BandScratch,
    work: &mut Work<'_>,
) -> Result<(), RasterError> {
    work.poll()?;
    let n = usize::from(poly.len);
    let points = &poly.points[..n];
    let (mut lo, mut hi) = (points[0], points[0]);
    for p in points {
        lo = [lo[0].min(p[0]), lo[1].min(p[1])];
        hi = [hi[0].max(p[0]), hi[1].max(p[1])];
    }
    let width = i32::try_from(region.width).map_err(|_| RasterError::Size)?;
    let top = i32::try_from(region.y0).map_err(|_| RasterError::Size)?;
    let bottom = top + i32::try_from(region.rows).map_err(|_| RasterError::Size)? - 1;
    let x0 = lo[0].div_euclid(SUBTEXEL).max(0);
    let x1 = hi[0].div_euclid(SUBTEXEL).min(width - 1);
    let rows = lo[1].div_euclid(SUBTEXEL).max(top)..=hi[1].div_euclid(SUBTEXEL).min(bottom);
    if x0 > x1 || rows.is_empty() {
        return Ok(());
    }
    let mut edges = [Edge::default(); MAX_POLY_VERTICES];
    for (i, edge) in edges[..n].iter_mut().enumerate() {
        let (a, b) = (points[i], points[(i + 1) % n]);
        let (dx, dy) = (
            i64::from(b[0]) - i64::from(a[0]),
            i64::from(b[1]) - i64::from(a[1]),
        );
        let top_left = dy < 0 || (dy == 0 && dx > 0);
        *edge = Edge {
            dx,
            dy,
            ax: i64::from(a[0]),
            ay: i64::from(a[1]),
            bias: i64::from(!top_left),
        };
    }
    let samples = region.offsets.len();
    let [gx, gy, base] = poly.plane.map(f64::from);
    let unit = f64::from(SUBTEXEL);
    let solid = u32::from_le_bytes([poly.rgb[0], poly.rgb[1], poly.rgb[2], 255]);
    for py in rows {
        let row = us((py - top).cast_unsigned()) * us(region.width);
        for (s, offset) in region.offsets.iter().enumerate() {
            let sy = py * SUBTEXEL + offset[1];
            if sy < lo[1] || sy > hi[1] {
                continue;
            }
            let Some((first, last)) = span(&edges[..n], sy, offset[0], x0, x1) else {
                continue;
            };
            work.spend((last - first + 1).cast_unsigned())?;
            let row_depth = gy * f64::from(sy) / unit + base;
            for px in first..=last {
                let key = key(gx * f64::from(px * SUBTEXEL + offset[0]) / unit + row_depth);
                let at = (row + us(px.cast_unsigned())) * samples + s;
                if key <= band.depth[at] {
                    continue;
                }
                if poly.opaque {
                    band.depth[at] = key;
                    band.color[at] = solid;
                    band.ids[at] = poly.solid;
                } else {
                    // shortcut: interpenetrating translucent solids may blend in the wrong order, until OSC-218 orders them.
                    band.color[at] = over(band.color[at], poly.rgb, poly.alpha);
                    if poly.picks {
                        band.ids[at] = poly.solid;
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default, Clone, Copy)]
struct Edge {
    dx: i64,
    dy: i64,
    ax: i64,
    ay: i64,
    bias: i64,
}

/// The texels whose sample at x offset `ox` on sample row `sy` passes every edge, exactly: each
/// edge's E(X) = dx·(sy − ay) − dy·(X − ax) ≥ bias bounds X = px·256 + ox from one side.
fn span(edges: &[Edge], sy: i32, ox: i32, x0: i32, x1: i32) -> Option<(i32, i32)> {
    let (unit, sy, ox) = (i64::from(SUBTEXEL), i64::from(sy), i64::from(ox));
    let (mut first, mut last) = (i64::from(x0), i64::from(x1));
    for e in edges {
        let k = e.dx * (sy - e.ay) + e.dy * e.ax - e.bias;
        match e.dy.cmp(&0) {
            std::cmp::Ordering::Equal if k < 0 => return None,
            std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Greater => {
                last = last.min((k.div_euclid(e.dy) - ox).div_euclid(unit));
            }
            std::cmp::Ordering::Less => {
                let x_min = -k.div_euclid(-e.dy);
                first = first.max(-(ox - x_min).div_euclid(unit));
            }
        }
    }
    let first = i32::try_from(first).ok()?;
    let last = i32::try_from(last).ok()?;
    (first <= last).then_some((first, last))
}

/// Source-over in premultiplied 8-bit, each channel rounded as `(s·α + d·(255 − α) + 127) / 255`.
fn over(dst: u32, rgb: [u8; 3], alpha: u8) -> u32 {
    let [dr, dg, db, da] = dst.to_le_bytes();
    let a = u16::from(alpha);
    let mix = |s: u8, d: u8| byte((u16::from(s) * a + u16::from(d) * (255 - a) + 127) / 255);
    u32::from_le_bytes([
        mix(rgb[0], dr),
        mix(rgb[1], dg),
        mix(rgb[2], db),
        mix(255, da),
    ])
}

fn byte(v: u16) -> u8 {
    u8::try_from(v).expect("a rounded 8-bit blend is at most 255")
}

fn resolve(
    region: &Region<'_>,
    band: &BandScratch,
    color: &mut [u32],
    ids: &mut [u8],
    work: &mut Work<'_>,
) -> Result<(), RasterError> {
    let samples = region.offsets.len();
    let width = us(region.width);
    for row in 0..us(region.rows) {
        work.poll()?;
        let out = (us(region.y0) + row) * width;
        for x in 0..width {
            let at = (row * width + x) * samples;
            let colors = &band.color[at..at + samples];
            let mut sum = [0_u16; 4];
            for c in colors {
                for (total, channel) in sum.iter_mut().zip(c.to_le_bytes()) {
                    *total += u16::from(channel);
                }
            }
            let count = u16::try_from(samples).expect("one or four samples");
            let [red, green, blue, alpha] = sum.map(|total| byte((total + count / 2) / count));
            color[out + x] = straight(red, green, blue, alpha);
            ids[out + x] = majority(&band.ids[at..at + samples]);
        }
    }
    Ok(())
}

fn straight(r: u8, g: u8, b: u8, a: u8) -> u32 {
    if a == 0 {
        return 0;
    }
    let a16 = u16::from(a);
    let un = |c: u8| byte(((u16::from(c) * 255 + a16 / 2) / a16).min(255));
    u32::from_le_bytes([un(b), un(g), un(r), a])
}

/// The most frequent id, background included; ties go to the id whose first sample is lowest.
#[expect(clippy::naive_bytecount, reason = "at most four samples")]
fn majority(ids: &[u8]) -> u8 {
    let mut best = (0, ids[0]);
    for &id in ids {
        let count = ids.iter().filter(|&&other| other == id).count();
        if count > best.0 {
            best = (count, id);
        }
    }
    best.1
}

/// Gives each clear texel its first covered neighbour's colour (left, right, up, down), keeping
/// alpha 0, so GPUI's straight-alpha bilinear sampler doesn't fringe a scaled image.
fn bleed(
    width: u32,
    height: u32,
    color: &mut [u32],
    work: &mut Work<'_>,
) -> Result<(), RasterError> {
    let (w, h) = (us(width), us(height));
    for y in 0..h {
        work.poll()?;
        for x in 0..w {
            let at = y * w + x;
            if color[at] >> 24 != 0 {
                continue;
            }
            let neighbours = [
                (x > 0).then(|| at - 1),
                (x + 1 < w).then_some(at + 1),
                (y > 0).then(|| at - w),
                (y + 1 < h).then_some(at + w),
            ];
            if let Some(source) = neighbours
                .into_iter()
                .flatten()
                .find(|&n| color[n] >> 24 != 0)
            {
                color[at] = color[source] & 0x00ff_ffff;
            }
        }
    }
    Ok(())
}

#[expect(
    clippy::as_conversions,
    reason = "u32 widens losslessly on every supported target"
)]
fn us(v: u32) -> usize {
    v as usize
}
