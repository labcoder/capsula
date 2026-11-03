//! The little vector math the renderer needs.

use std::ops::{Add, Mul, Sub};

/// A point or offset in the plane, in cloud units or pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Vec2 {
    pub(crate) x: f32,
    pub(crate) y: f32,
}

/// A point or direction in cloud space: x right, y up, z toward the viewer.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Vec3 {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) z: f32,
}

pub(crate) const fn v2(x: f32, y: f32) -> Vec2 {
    Vec2 { x, y }
}

pub(crate) const fn v3(x: f32, y: f32, z: f32) -> Vec3 {
    Vec3 { x, y, z }
}

/// A colour in display (sRGB-encoded) space, each channel 0..=1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Rgb {
    pub(crate) r: f32,
    pub(crate) g: f32,
    pub(crate) b: f32,
}

/// A colour from a 0xRRGGBB literal.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "a byte fits f32 exactly, and From isn't const"
)]
pub(crate) const fn hex(c: u32) -> Rgb {
    Rgb {
        r: ((c >> 16) & 0xff) as f32 / 255.0,
        g: ((c >> 8) & 0xff) as f32 / 255.0,
        b: (c & 0xff) as f32 / 255.0,
    }
}

impl Rgb {
    pub(crate) fn lerp(self, o: Self, t: f32) -> Self {
        Self {
            r: mix(self.r, o.r, t),
            g: mix(self.g, o.g, t),
            b: mix(self.b, o.b, t),
        }
    }

    /// Screen blend: brightens like light added from inside, never past white.
    pub(crate) fn screen(self, light: Self, k: f32) -> Self {
        let s = |a: f32, b: f32| 1.0 - (1.0 - a) * (1.0 - (b * k).clamp(0.0, 1.0));
        Self {
            r: s(self.r, light.r),
            g: s(self.g, light.g),
            b: s(self.b, light.b),
        }
    }
}

impl Vec2 {
    pub(crate) fn len(self) -> f32 {
        self.x.hypot(self.y)
    }

    pub(crate) fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y
    }

    /// Each component raised to at least `s`.
    pub(crate) fn max(self, s: f32) -> Self {
        v2(self.x.max(s), self.y.max(s))
    }
}

impl Add for Vec2 {
    type Output = Self;

    fn add(self, o: Self) -> Self {
        v2(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Vec2 {
    type Output = Self;

    fn sub(self, o: Self) -> Self {
        v2(self.x - o.x, self.y - o.y)
    }
}

impl Vec3 {
    pub(crate) fn len(self) -> f32 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    pub(crate) fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    /// The unit vector; a zero vector stays zero rather than becoming NaN.
    pub(crate) fn normalized(self) -> Self {
        let l = self.len().max(1e-6);
        v3(self.x / l, self.y / l, self.z / l)
    }
}

impl Add for Vec3 {
    type Output = Self;

    fn add(self, o: Self) -> Self {
        v3(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for Vec3 {
    type Output = Self;

    fn sub(self, o: Self) -> Self {
        v3(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Self;

    fn mul(self, s: f32) -> Self {
        v3(self.x * s, self.y * s, self.z * s)
    }
}

pub(crate) fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Hermite step from 0 at `e0` to 1 at `e1`.
pub(crate) fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Polynomial smooth minimum: unions shapes with a fillet of width `k`, so lobes melt together.
pub(crate) fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = (0.5 + 0.5 * (b - a) / k).clamp(0.0, 1.0);
    mix(b, a, h) - k * h * (1.0 - h)
}

/// Smooth maximum, the intersection counterpart of [`smin`].
pub(crate) fn smax(a: f32, b: f32, k: f32) -> f32 {
    -smin(-a, -b, k)
}

/// A cheap integer hash to 0..=1.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "24 bits fit f32's mantissa exactly"
)]
pub(crate) fn hash(mut x: u32) -> f32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    (x & 0x00ff_ffff) as f32 / 16_777_215.0
}

#[cfg(test)]
mod tests {
    use super::{hash, hex, smax, smin, smoothstep, v3};

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "past the fillet the result is exactly one input"
    )]
    fn smooth_min_and_max_are_exact_once_apart_by_the_fillet() {
        assert_eq!(
            smin(0.0, 1.0, 0.5),
            0.0,
            "past the fillet it's the plain min"
        );
        assert_eq!(
            smax(0.0, 1.0, 0.5),
            1.0,
            "past the fillet it's the plain max"
        );
        assert_eq!(
            smin(0.0, 0.0, 0.5),
            -0.125,
            "equal inputs dig a quarter of the fillet"
        );
    }

    #[test]
    fn a_zero_vector_normalizes_to_zero() {
        assert_eq!(v3(0.0, 0.0, 0.0).normalized(), v3(0.0, 0.0, 0.0));
        assert_eq!(v3(0.0, 3.0, 4.0).normalized(), v3(0.0, 0.6, 0.8));
    }

    #[test]
    fn hex_splits_channels() {
        let c = hex(0xFF_00_33);
        assert_eq!((c.r, c.g), (1.0, 0.0));
        assert!((c.b - 0.2).abs() < 1e-7, "0x33 is 51/255: {}", c.b);
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "the step clamps to exactly 0 and 1")]
    fn smoothstep_clamps_and_centres() {
        assert_eq!(smoothstep(0.0, 1.0, -1.0), 0.0);
        assert_eq!(smoothstep(0.0, 1.0, 2.0), 1.0);
        assert_eq!(smoothstep(0.0, 1.0, 0.5), 0.5);
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "zero hashes to exactly zero")]
    fn the_hash_stays_in_unit_range() {
        assert_eq!(hash(0), 0.0, "zero is a fixed point of every step");
        for x in [1, 7, 0x9e37, u32::MAX] {
            let h = hash(x);
            assert!((0.0..=1.0).contains(&h), "hash({x}) = {h}");
        }
    }
}
