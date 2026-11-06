//! Validated orbit cameras and flat directional lighting, calculated in f64.

use super::frame::FrameError;
use crate::record::View3d;

/// A private, finite orbit state; angles are degrees and the vertical field of view is 10–120°.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    angles: [f64; 2],
    dist: f64,
    fov: f64,
    target: [f64; 3],
}

impl Camera {
    /// Constructs a camera without clamping authored distance.
    /// # Errors
    /// Returns `Invalid` for non-finite inputs, pitch outside ±85°, distance ≤0 or invalid fov.
    pub fn new(
        yaw: f64,
        pitch: f64,
        dist: f64,
        fov: f64,
        target: [f64; 3],
    ) -> Result<Self, FrameError> {
        if ![yaw, pitch, dist, fov]
            .into_iter()
            .chain(target)
            .all(f64::is_finite)
            || !(-85.0..=85.0).contains(&pitch)
            || dist <= 0.0
            || !(10.0..=120.0).contains(&fov)
        {
            return Err(FrameError::Invalid);
        }
        Ok(Self {
            angles: [yaw.rem_euclid(360.0), pitch],
            dist,
            fov,
            target,
        })
    }

    /// Applies record fields over 30° yaw, 20° pitch, 40° fov and the world bounds' centre.
    /// # Errors
    /// Returns `Invalid` for non-finite, inverted or zero-radius bounds or invalid defaults.
    pub fn authored(view: &View3d, bounds: [[f64; 3]; 2]) -> Result<Self, FrameError> {
        let [min, max] = bounds;
        if !(0..3).all(|i| min[i].is_finite() && max[i].is_finite() && min[i] <= max[i]) {
            return Err(FrameError::Invalid);
        }
        let target = std::array::from_fn(|i| min[i] / 2.0 + max[i] / 2.0);
        let radius = (0..3)
            .map(|i| ((max[i] - min[i]) / 2.0).powi(2))
            .sum::<f64>()
            .sqrt();
        if !radius.is_finite() || radius <= 0.0 {
            return Err(FrameError::Invalid);
        }
        let c = view.camera.as_ref();
        Self::new(
            c.and_then(|c| c.yaw).map_or(30.0, |v| f64::from(v.get())),
            c.and_then(|c| c.pitch).map_or(20.0, |v| f64::from(v.get())),
            c.and_then(|c| c.dist)
                .map_or(3.0 * radius, |v| f64::from(v.get())),
            c.and_then(|c| c.fov).map_or(40.0, |v| f64::from(v.get())),
            c.and_then(|c| c.target)
                .map_or(target, |v| v.map(|v| f64::from(v.get()))),
        )
    }

    /// Orbits by logical-pixel deltas at 0.4°/px, clamping distance to 1.2–6 radii.
    /// # Errors
    /// Returns `Invalid` without changing the camera for invalid deltas, radius or arithmetic.
    pub fn orbit(&mut self, dx: f64, dy: f64, radius: f64) -> Result<(), FrameError> {
        if ![dx, dy, radius, 6.0 * radius]
            .into_iter()
            .all(f64::is_finite)
            || radius <= 0.0
        {
            return Err(FrameError::Invalid);
        }
        *self = Self::new(
            self.angles[0] - 0.4 * dx,
            (self.angles[1] + 0.4 * dy).clamp(-85.0, 85.0),
            self.dist.clamp(1.2 * radius, 6.0 * radius),
            self.fov,
            self.target,
        )?;
        Ok(())
    }

    /// The same camera turned about its target by `degrees` of yaw.
    #[must_use]
    pub fn yawed(self, degrees: f64) -> Self {
        Self {
            angles: [(self.angles[0] + degrees).rem_euclid(360.0), self.angles[1]],
            ..self
        }
    }

    /// Yaw, pitch, distance and vertical fov, in that order.
    pub fn parameters(self) -> [f64; 4] {
        [self.angles[0], self.angles[1], self.dist, self.fov]
    }

    /// Right, up and forward unit vectors, independent of rounded eye coordinates.
    pub fn basis(self) -> [[f64; 3]; 3] {
        let [(sy, cy), (sp, cp)] = self.angles.map(|a| a.to_radians().sin_cos());
        [
            [cy, 0.0, -sy],
            [-sp * sy, cp, -sp * cy],
            [-cp * sy, -sp, -cp * cy],
        ]
    }

    /// World-space eye, never narrowed to f32.
    pub fn eye(self) -> [f64; 3] {
        let forward = self.basis()[2];
        std::array::from_fn(|i| self.target[i] - self.dist * forward[i])
    }

    pub(super) fn focal(self, height: u32) -> f64 {
        f64::from(height) / (2.0 * (self.fov.to_radians() / 2.0).tan())
    }

    /// Near world distance, with a precision floor chosen using the internal raster's focal length.
    /// # Errors
    /// Returns `Invalid` for invalid radius, coordinate magnitude or height, `Numeric` on overflow.
    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        reason = "the f32 spacing is the explicit world-coordinate precision contract"
    )]
    pub fn near(self, radius: f64, largest: f64, height: u32) -> Result<f64, FrameError> {
        if !radius.is_finite()
            || radius <= 0.0
            || !largest.is_finite()
            || largest < 0.0
            || height == 0
        {
            return Err(FrameError::Invalid);
        }
        let c = largest as f32;
        let floor = 8.0 * f64::from(c.next_up() - c) * self.focal(height);
        let near = (0.05 * self.dist).max(1e-4 * radius).max(floor);
        if !floor.is_finite() || !near.is_finite() || near <= 0.0 {
            return Err(FrameError::Numeric);
        }
        Ok(near)
    }
}

/// Normalised world-space direction and ambient fraction; no material or label text is retained.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Light {
    dir: [f64; 3],
    ambient: f64,
}

impl Light {
    /// Normalises a nonzero direction once.
    /// # Errors
    /// Returns `Invalid` for a zero/non-finite direction or ambient outside 0..=1.
    pub fn new(dir: [f64; 3], ambient: f64) -> Result<Self, FrameError> {
        let length = dir[0].hypot(dir[1]).hypot(dir[2]);
        if !length.is_finite() || length == 0.0 || !(0.0..=1.0).contains(&ambient) {
            return Err(FrameError::Invalid);
        }
        Ok(Self {
            dir: dir.map(|v| v / length),
            ambient,
        })
    }

    /// Resolves S1's defaults: direction (−1, −2, −1), ambient 0.35.
    pub fn authored(view: &View3d) -> Self {
        let light = view.light.as_ref();
        let dir = light
            .and_then(|l| l.dir)
            .map_or([-1.0, -2.0, -1.0], |v| v.get().map(|v| f64::from(v.get())));
        let ambient = light
            .and_then(|l| l.ambient)
            .map_or(0.35, |v| f64::from(v.get()));
        Self::new(dir, ambient).unwrap_or_else(|_| Self {
            dir: [-1.0, -2.0, -1.0].map(|v| v / 6.0_f64.sqrt()),
            ambient: 0.35,
        })
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped lighting times a byte rounds to a byte"
    )]
    pub(super) fn shade(self, normal: [f64; 3], rgb: [u8; 3]) -> [u8; 3] {
        let length = normal[0].hypot(normal[1]).hypot(normal[2]);
        let diffuse = -(0..3)
            .map(|i| normal[i] / length * self.dir[i])
            .sum::<f64>();
        let intensity = self.ambient + (1.0 - self.ambient) * diffuse.clamp(0.0, 1.0);
        rgb.map(|v| (f64::from(v) * intensity).round() as u8)
    }
}
