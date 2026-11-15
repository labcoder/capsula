//! Kumo's body as a 3D implicit surface: a softly flat base melted into a few top lobes, baked
//! once per pixel size into a G-buffer of coverage, normal, thickness and occlusion. Animation
//! never re-bakes; it samples the bake through a transform.

use super::math::{Vec2, Vec3, hash, mix, smax, smin, v2, v3};

/// A sphere-like lobe, flattened in depth.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Lobe {
    pub(crate) center: Vec2,
    pub(crate) radius: f32,
}

/// The body in cloud units: x right, y up, the view looking down -z. The base sits on the floor.
#[derive(Clone, Debug, PartialEq)]
pub struct Silhouette {
    /// The rounded base slab: its centre, half extents and rounding.
    pub(crate) base_center: Vec2,
    pub(crate) base_half: Vec3,
    pub(crate) base_round: f32,
    pub(crate) lobes: Vec<Lobe>,
    /// Fillet width where lobes meet: wide enough that no circle edge shows.
    pub(crate) blend: f32,
    /// The softly flat underside.
    pub(crate) floor: f32,
    /// Depth squash: 1 is round, lower is a flatter pillow.
    pub(crate) depth: f32,
    /// Vapour detail: the amplitude, in cloud units, and frequency of a small surface noise.
    pub(crate) detail: f32,
    pub(crate) detail_freq: f32,
}

impl Silhouette {
    /// The emoji-like cloud: a wide base melted into an off-centre crown and two shoulders, with
    /// fillets broad enough that it reads as one mass rather than circles on a slab.
    pub fn emoji() -> Self {
        let lobe = |x, y, radius| Lobe {
            center: v2(x, y),
            radius,
        };
        Self {
            base_center: v2(0.0, -0.20),
            base_half: v3(0.66, 0.17, 0.33),
            base_round: 0.17,
            lobes: vec![
                lobe(-0.50, -0.06, 0.30),
                lobe(-0.05, 0.16, 0.43),
                lobe(0.48, -0.075, 0.31),
                // The underside: two soft bellies instead of a level shelf.
                lobe(-0.27, -0.21, 0.25),
                lobe(0.22, -0.22, 0.25),
            ],
            blend: 0.14,
            floor: -0.52,
            depth: 0.82,
            detail: 0.01,
            detail_freq: 4.8,
        }
    }

    /// Signed distance: negative inside, approximate below the surface, conservative above it.
    pub(crate) fn sdf(&self, p: Vec3) -> f32 {
        let z = p.z / self.depth;
        let local = v3(p.x - self.base_center.x, p.y - self.base_center.y, z);
        let round = self.base_round;
        let excess = v3(
            local.x.abs() - self.base_half.x + round,
            local.y.abs() - self.base_half.y + round,
            local.z.abs() - self.base_half.z + round,
        );
        let outside = v3(excess.x.max(0.0), excess.y.max(0.0), excess.z.max(0.0)).len();
        let mut dist = outside + excess.x.max(excess.y).max(excess.z).min(0.0) - round;
        for lobe in &self.lobes {
            let from = v3(p.x - lobe.center.x, p.y - lobe.center.y, z);
            dist = smin(dist, from.len() - lobe.radius, self.blend);
        }
        // Front face only: the z = 0 slice must stay the exact silhouette the bake reads coverage
        // from, or the rim flickers between hits and misses. So that slice skips the noise.
        let front = (z.abs() / 0.18).clamp(0.0, 1.0);
        if self.detail > 0.0 && front > 0.0 {
            dist += self.detail * front * front * fbm(p * self.detail_freq);
        }
        smax(dist, self.floor - p.y, 0.14) * self.depth.min(1.0)
    }

    fn normal(&self, p: Vec3, e: f32) -> Vec3 {
        let dx = self.sdf(p + v3(e, 0.0, 0.0)) - self.sdf(p - v3(e, 0.0, 0.0));
        let dy = self.sdf(p + v3(0.0, e, 0.0)) - self.sdf(p - v3(0.0, e, 0.0));
        let dz = self.sdf(p + v3(0.0, 0.0, e)) - self.sdf(p - v3(0.0, 0.0, e));
        v3(dx, dy, dz).normalized()
    }
}

/// Three octaves of smooth value noise in -1..=1.
pub(crate) fn fbm(p: Vec3) -> f32 {
    let mut sum = 0.0;
    let mut amp = 0.5;
    let mut q = p;
    for _ in 0..3 {
        sum += amp * value_noise(q);
        q = q * 2.03;
        amp *= 0.5;
    }
    sum / 0.875
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "lattice coordinates near the body are small integers in f32"
)]
fn value_noise(p: Vec3) -> f32 {
    let (fx, fy, fz) = (p.x.floor(), p.y.floor(), p.z.floor());
    let s = |t: f32| t * t * (3.0 - 2.0 * t);
    let (tx, ty, tz) = (s(p.x - fx), s(p.y - fy), s(p.z - fz));
    let h = |dx: f32, dy: f32, dz: f32| {
        let n = ((fx + dx) as i32).wrapping_mul(73_856_093)
            ^ ((fy + dy) as i32).wrapping_mul(19_349_663)
            ^ ((fz + dz) as i32).wrapping_mul(83_492_791);
        hash(n.cast_unsigned()) * 2.0 - 1.0
    };
    let x00 = mix(h(0.0, 0.0, 0.0), h(1.0, 0.0, 0.0), tx);
    let x10 = mix(h(0.0, 1.0, 0.0), h(1.0, 1.0, 0.0), tx);
    let x01 = mix(h(0.0, 0.0, 1.0), h(1.0, 0.0, 1.0), tx);
    let x11 = mix(h(0.0, 1.0, 1.0), h(1.0, 1.0, 1.0), tx);
    mix(mix(x00, x10, ty), mix(x01, x11, ty), tz)
}

/// Where the cloud sits in a square buffer: how many cloud units it spans, and its centre.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Framing {
    pub(crate) span: f32,
    pub(crate) center: Vec2,
    /// A soft vapour band about a pixel wide at the edge, rather than a crisp one.
    pub(crate) soft: bool,
}

impl Framing {
    /// Room for the shadow underneath; the body fills about 78% of the width.
    pub(crate) const CARD: Self = Self {
        span: 2.2,
        center: v2(0.0, 0.06),
        soft: true,
    };

    /// The cloud-space point at fractional pixel (i, j) of a square `size` pixels wide.
    pub(crate) fn to_cloud(self, size: u16, i: f32, j: f32) -> Vec2 {
        let half = f32::from(size) * 0.5;
        let s = self.units_per_pixel(size);
        v2(
            self.center.x + (i - half) * s,
            self.center.y - (j - half) * s,
        )
    }

    /// The fractional pixel of a cloud-space point: the inverse of [`Self::to_cloud`].
    pub(crate) fn to_pixel(self, size: u16, p: Vec2) -> Vec2 {
        let half = f32::from(size) * 0.5;
        let s = f32::from(size) / self.span;
        v2(
            (p.x - self.center.x) * s + half,
            (self.center.y - p.y) * s + half,
        )
    }

    /// Cloud units per pixel. A zero-sized square is treated as one pixel, so no caller sees
    /// an infinity.
    pub(crate) fn units_per_pixel(self, size: u16) -> f32 {
        self.span / f32::from(size.max(1))
    }
}

/// One baked pixel of the body.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Texel {
    /// Antialiased silhouette coverage, 0..=1.
    pub(crate) coverage: f32,
    pub(crate) normal: Vec3,
    /// Front-surface depth: 0 at the rim, 1 at the thickest point.
    pub(crate) thickness: f32,
    /// Ambient occlusion from the surface field, darker in the creases between lobes: 1 is open.
    pub(crate) occlusion: f32,
}

/// The baked body for one square pixel size, row-major from the top-left.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GBuffer {
    pub(crate) size: u16,
    pub(crate) texels: Vec<Texel>,
}

impl GBuffer {
    /// Ray-marches the body once per covered pixel, up to 96 steps each. Its cost grows with the
    /// size squared, so it runs when the size or scale changes, never per frame or in paint.
    pub(crate) fn bake(shape: &Silhouette, size: u16, framing: Framing) -> Self {
        let upp = framing.units_per_pixel(size);
        let band = if framing.soft { upp * 0.9 + 0.008 } else { upp };
        let max_z = shape
            .lobes
            .iter()
            .map(|l| l.radius)
            .fold(shape.base_half.z, f32::max)
            * shape.depth;
        let mut texels = Vec::with_capacity(usize::from(size) * usize::from(size));
        for j in 0..size {
            for i in 0..size {
                let c = framing.to_cloud(size, f32::from(i) + 0.5, f32::from(j) + 0.5);
                texels.push(texel(shape, c, upp, band, max_z));
            }
        }
        Self { size, texels }
    }
}

/// One pixel of the bake at cloud point `at`.
fn texel(shape: &Silhouette, at: Vec2, upp: f32, band: f32, max_z: f32) -> Texel {
    // Every part is centred on z = 0, so the slice there is the exact silhouette.
    let edge = shape.sdf(v3(at.x, at.y, 0.0));
    let coverage = (0.5 - edge / band).clamp(0.0, 1.0);
    if coverage <= 0.0 {
        return Texel::default();
    }
    let mut depth = max_z + 0.05;
    let mut hit = false;
    if edge < 0.0 {
        for _ in 0..96 {
            let dist = shape.sdf(v3(at.x, at.y, depth));
            if dist < 1e-4 {
                hit = true;
                break;
            }
            depth -= (dist * 0.9).max(2e-4);
            if depth < 0.0 {
                break;
            }
        }
    }
    let surface = v3(at.x, at.y, if hit { depth.max(0.0) } else { 0.0 });
    let normal = shape.normal(surface, upp * 0.5);
    let occlusion: f32 = (1..=3_u8)
        .map(|step| {
            let step = f32::from(step);
            let reach = 0.07 * step;
            (reach - shape.sdf(surface + normal * reach)).max(0.0) / step
        })
        .sum();
    Texel {
        coverage,
        normal,
        thickness: if hit {
            (depth / max_z).clamp(0.0, 1.0)
        } else {
            0.0
        },
        occlusion: (1.0 - occlusion * 3.0).clamp(0.0, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::{Framing, GBuffer, Silhouette, Texel, v2, v3};

    #[test]
    fn the_sdf_is_negative_inside_and_positive_outside() {
        let shape = Silhouette::emoji();
        assert!(shape.sdf(v3(-0.05, 0.16, 0.0)) < 0.0, "the crown's centre");
        assert!(shape.sdf(v3(0.0, -0.25, 0.0)) < 0.0, "the base");
        assert!(shape.sdf(v3(2.0, 0.0, 0.0)) > 0.0, "far to the right");
        assert!(shape.sdf(v3(0.0, -0.7, 0.0)) > 0.0, "under the floor");
        assert!(shape.sdf(v3(0.0, 0.0, 1.0)) > 0.0, "in front of the body");
    }

    #[test]
    fn the_crown_top_is_on_the_surface() {
        // The crown lobe is centred at (-0.05, 0.16) with radius 0.43, and nothing else is
        // within a fillet of its top.
        let shape = Silhouette::emoji();
        assert!(shape.sdf(v3(-0.05, 0.59, 0.0)).abs() < 1e-5, "on the edge");
        assert!(shape.sdf(v3(-0.05, 0.58, 0.0)) < 0.0, "just inside");
        assert!(shape.sdf(v3(-0.05, 0.60, 0.0)) > 0.0, "just outside");
    }

    #[test]
    fn the_framing_centres_the_cloud_in_the_square() {
        let f = Framing::CARD;
        assert_eq!(f.to_cloud(64, 32.0, 32.0), v2(0.0, 0.06));
        let corner = f.to_cloud(64, 0.0, 0.0);
        assert!(
            (corner.x + 1.1).abs() < 1e-5 && (corner.y - 1.16).abs() < 1e-5,
            "the top-left corner: {corner:?}"
        );
        let back = f.to_pixel(64, corner);
        assert!(
            back.x.abs() < 1e-4 && back.y.abs() < 1e-4,
            "to_pixel inverts to_cloud: {back:?}"
        );
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "coverage is clamped to exactly 1")]
    fn the_bake_covers_the_middle_and_leaves_the_corners_empty() {
        let bake = GBuffer::bake(&Silhouette::emoji(), 32, Framing::CARD);
        assert_eq!(bake.texels.len(), 32 * 32);
        for corner in [0, 31, 31 * 32, 32 * 32 - 1] {
            assert_eq!(bake.texels[corner], Texel::default(), "corner {corner}");
        }
        let middle = bake.texels[16 * 32 + 16];
        assert_eq!(middle.coverage, 1.0);
        assert!(middle.thickness > 0.5, "thick in the middle: {middle:?}");
        assert!(middle.normal.z > 0.9, "facing the viewer: {middle:?}");
        assert!(
            (0.0..=1.0).contains(&middle.occlusion),
            "occlusion in range: {middle:?}"
        );
    }

    #[test]
    fn the_soft_band_has_partial_coverage_and_the_crisp_one_less() {
        let shape = Silhouette::emoji();
        let partial = |framing| {
            GBuffer::bake(&shape, 48, framing)
                .texels
                .iter()
                .filter(|t| t.coverage > 0.0 && t.coverage < 1.0)
                .count()
        };
        let soft = partial(Framing::CARD);
        let crisp = partial(Framing {
            soft: false,
            ..Framing::CARD
        });
        assert!(crisp > 0, "even a crisp edge is antialiased");
        assert!(soft > crisp, "soft {soft} vs crisp {crisp}");
    }

    #[test]
    fn the_bake_is_deterministic() {
        let shape = Silhouette::emoji();
        assert_eq!(
            GBuffer::bake(&shape, 24, Framing::CARD),
            GBuffer::bake(&shape, 24, Framing::CARD)
        );
    }

    #[test]
    fn the_outer_strips_are_empty_and_every_channel_is_finite() {
        let bake = GBuffer::bake(&Silhouette::emoji(), 32, Framing::CARD);
        for k in 0..32 {
            for (i, j) in [(k, 0), (k, 31), (0, k), (31, k)] {
                assert_eq!(
                    bake.texels[j * 32 + i],
                    Texel::default(),
                    "edge texel {i},{j}"
                );
            }
        }
        for t in &GBuffer::bake(&Silhouette::emoji(), 64, Framing::CARD).texels {
            let channels = [
                t.coverage,
                t.normal.x,
                t.normal.y,
                t.normal.z,
                t.thickness,
                t.occlusion,
            ];
            assert!(channels.iter().all(|c| c.is_finite()), "{t:?}");
        }
    }

    #[test]
    fn a_zero_size_has_a_finite_scale() {
        assert!((Framing::CARD.units_per_pixel(0) - 2.2).abs() < 1e-6);
        let p = Framing::CARD.to_cloud(0, 0.0, 0.0);
        assert!(p.x.is_finite() && p.y.is_finite(), "{p:?}");
    }

    #[test]
    fn empty_and_single_pixel_bakes() {
        let shape = Silhouette::emoji();
        assert!(GBuffer::bake(&shape, 0, Framing::CARD).texels.is_empty());
        let one = GBuffer::bake(&shape, 1, Framing::CARD);
        assert_eq!(one.texels.len(), 1);
        assert!(
            one.texels[0].coverage > 0.5,
            "its centre is inside the body: {:?}",
            one.texels[0]
        );
    }
}
