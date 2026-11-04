//! Strict 3D records and shape-dependent geometry validation, without rendering policy.
use super::Color;
use crate::{
    Budget, ErrorCode, Finite, Text, WireId,
    limits::{MAX_COORD, MAX_CURVED_SEGMENTS, MAX_PATH_POINTS},
    text::Bounded,
};
use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! scalar {
    ($name:ident, $doc:literal, $valid:expr) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
        #[serde(try_from = "f32", into = "f32")]
        pub struct $name(f32);
        impl $name {
            /// Returns the checked value.
            pub fn get(self) -> f32 {
                self.0
            }
        }
        impl TryFrom<f32> for $name {
            type Error = ErrorCode;
            fn try_from(value: f32) -> Result<Self, Self::Error> {
                if !value.is_finite() {
                    return Err(ErrorCode::NotFinite);
                }
                if !($valid)(value) {
                    return Err(ErrorCode::OutOfRange);
                }
                Ok(Self(value))
            }
        }
        impl From<$name> for f32 {
            fn from(value: $name) -> Self {
                value.0
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "(..)"))
            }
        }
    };
}
scalar!(Size, "A geometry size in 0.001..=20,000.", |v| {
    (0.001..=MAX_COORD).contains(&v)
});
scalar!(Length, "A radius that is zero or in 0.001..=20,000.", |v| {
    v == 0.0 || (0.001..=MAX_COORD).contains(&v)
});
scalar!(Unit, "A unit interval value in 0..=1.", |v| {
    (0.0..=1.0).contains(&v)
});
scalar!(Fov, "A camera field of view in 10..=120 degrees.", |v| {
    (10.0..=120.0).contains(&v)
});
scalar!(Pitch, "A camera pitch in -85..=85 degrees.", |v| {
    (-85.0..=85.0).contains(&v)
});
scalar!(
    Spin,
    "An orbit speed in -360..=360 degrees per second.",
    |v| (-360.0..=360.0).contains(&v)
);

/// An integer subdivision count in 3..=48.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Segments(u8);
impl Segments {
    /// Returns the checked count.
    pub fn get(self) -> u8 {
        self.0
    }
}
impl TryFrom<u8> for Segments {
    type Error = ErrorCode;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if !(3..=MAX_CURVED_SEGMENTS).contains(&usize::from(value)) {
            return Err(ErrorCode::OutOfRange);
        }
        Ok(Self(value))
    }
}
impl From<Segments> for u8 {
    fn from(value: Segments) -> Self {
        value.0
    }
}
impl fmt::Debug for Segments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Segments").field(&self.0).finish()
    }
}

/// A 3D viewport; omitted camera policy is resolved by the camera module, not decoding.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View3d {
    /// Model identity.
    pub id: WireId,
    /// Logical viewport rectangle; absent means the scene canvas.
    #[serde(rename = "box")]
    pub bounds: Option<[Finite; 4]>,
    /// Explicit camera fields, without defaults.
    pub camera: Option<Camera>,
    /// Explicit lighting fields, without defaults.
    pub light: Option<Light>,
    /// Whether pointer orbit is enabled, defaulting to true.
    #[serde(default = "yes")]
    pub orbit: bool,
    /// Automatic orbit speed in degrees per second.
    pub spin: Option<Spin>,
}
fn yes() -> bool {
    true
}
impl fmt::Debug for View3d {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("View3d")
            .field("camera", &self.camera.is_some())
            .field("light", &self.light.is_some())
            .finish_non_exhaustive()
    }
}
/// Optional camera inputs; absence has no decode-time defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Camera {
    /// Orbit yaw in degrees.
    pub yaw: Option<Finite>,
    /// Orbit pitch in degrees.
    pub pitch: Option<Pitch>,
    /// Distance from the target.
    pub dist: Option<Size>,
    /// Vertical field of view in degrees.
    pub fov: Option<Fov>,
    /// World-space target.
    pub target: Option<[Finite; 3]>,
}
/// Optional directional and ambient lighting inputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Light {
    /// World-space light direction.
    pub dir: Option<[Finite; 3]>,
    /// Ambient intensity.
    pub ambient: Option<Unit>,
}
/// Append-only solid vocabulary, independent of the 2D shape kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SolidShape {
    /// Rectangular box.
    Box,
    /// Sphere.
    Sphere,
    /// Cylinder.
    Cylinder,
    /// Cone or truncated cone.
    Cone,
    /// Torus.
    Torus,
    /// Extruded outline.
    Extrude,
    /// Revolved profile.
    Lathe,
    /// Tube along a path.
    Tube,
}
/// A decoded solid; call [`Solid::geometry`] before using its shape fields.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Solid {
    /// Owning viewport identity.
    #[serde(rename = "in")]
    pub parent: WireId,
    /// Model identity.
    pub id: WireId,
    /// Geometry kind.
    pub shape: SolidShape,
    /// Box dimensions.
    pub size: Option<[Size; 3]>,
    /// Primary radius.
    pub r: Option<Size>,
    /// Cone top radius, torus tube radius or tube thickness.
    pub r2: Option<Length>,
    /// Cylinder or cone height.
    pub h: Option<Size>,
    /// Extrusion depth.
    pub depth: Option<Size>,
    /// Extrude outline or lathe profile; empty and omitted are equivalent.
    #[serde(default)]
    pub points: Bounded<[Finite; 2], MAX_PATH_POINTS>,
    /// Tube centerline; empty and omitted are equivalent.
    #[serde(default)]
    pub path: Bounded<[Finite; 3], MAX_PATH_POINTS>,
    /// Translation.
    pub pos: Option<[Finite; 3]>,
    /// Euler rotation in degrees.
    pub rot: Option<[Finite; 3]>,
    /// Uniform scale.
    pub scale: Option<Size>,
    /// Curved subdivision count, invalid on a box.
    pub segments: Option<Segments>,
    /// Optional attachment identity.
    pub attach: Option<WireId>,
    /// Display label.
    pub label: Option<Text>,
    /// Descriptive text.
    pub info: Option<Text>,
    /// Surface color.
    pub color: Option<Color>,
    /// Surface opacity.
    pub opacity: Option<Unit>,
}
impl fmt::Debug for Solid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Solid")
            .field("shape", &self.shape)
            .field("points", &self.points.len())
            .field("path", &self.path.len())
            .finish_non_exhaustive()
    }
}
/// Shape fields resolved for tessellation; degeneracy checks belong to the tessellator.
#[derive(Clone, Copy, PartialEq)]
pub enum Geometry<'a> {
    /// Box dimensions.
    Box {
        /// Dimensions along each axis.
        size: [f32; 3],
    },
    /// Sphere radius.
    Sphere {
        /// Sphere radius.
        r: f32,
    },
    /// Cylinder radius and height.
    Cylinder {
        /// Cylinder radius.
        r: f32,
        /// Cylinder height.
        h: f32,
    },
    /// Cone base, top and height; an omitted top is zero.
    Cone {
        /// Base radius.
        r: f32,
        /// Top radius, including zero for an apex.
        top: f32,
        /// Cone height.
        h: f32,
    },
    /// Torus ring and tube radii.
    Torus {
        /// Ring radius.
        ring: f32,
        /// Tube radius.
        tube: f32,
    },
    /// Outline and extrusion depth.
    Extrude {
        /// Closed outline vertices.
        outline: &'a [[Finite; 2]],
        /// Extrusion depth.
        depth: f32,
    },
    /// Radius/height profile.
    Lathe {
        /// Radius/height samples.
        profile: &'a [[Finite; 2]],
    },
    /// Centerline and tube thickness.
    Tube {
        /// Centerline vertices.
        path: &'a [[Finite; 3]],
        /// Tube radius.
        thickness: f32,
    },
}
impl fmt::Debug for Geometry<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Geometry(..)")
    }
}
/// A content-free failure in shape-dependent validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {code}")]
pub struct ShapeError {
    /// The invalid shape field.
    pub field: &'static str,
    /// Stable validation code.
    pub code: ErrorCode,
}
impl Solid {
    /// Resolves only fields used by this shape, without allocating or testing degeneracy.
    /// # Errors
    /// Rejects missing or unused fields, point counts and shape-dependent radius relations.
    /// # Panics
    /// Panics if the internal required-field schema disagrees with the shape resolver.
    pub fn geometry(&self) -> Result<Geometry<'_>, ShapeError> {
        use SolidShape::{Box, Cone, Cylinder, Extrude, Lathe, Sphere, Torus, Tube};
        let fail = |field, code| ShapeError { field, code };
        let required: &[&str] = match self.shape {
            Box => &["size"],
            Sphere => &["r"],
            Cylinder | Cone => &["r", "h"],
            Torus => &["r", "r2"],
            Extrude => &["points", "depth"],
            Lathe => &["points"],
            Tube => &["path", "r2"],
        };
        let fields = [
            ("size", self.size.is_some()),
            ("r", self.r.is_some()),
            ("r2", self.r2.is_some()),
            ("h", self.h.is_some()),
            ("depth", self.depth.is_some()),
            ("points", !self.points.is_empty()),
            ("path", !self.path.is_empty()),
            ("segments", self.segments.is_some()),
        ];
        for &field in required {
            if !fields.contains(&(field, true)) {
                return Err(fail(field, ErrorCode::MissingField));
            }
        }
        for (field, present) in fields {
            let used = required.contains(&field)
                || (field == "r2" && self.shape == Cone)
                || (field == "segments" && self.shape != Box);
            if present && !used {
                return Err(fail(field, ErrorCode::UnknownField));
            }
        }
        match self.shape {
            Extrude if self.points.len() < 3 => return Err(fail("points", ErrorCode::OutOfRange)),
            Extrude if self.points.len() > 128 => {
                return Err(fail("points", ErrorCode::OverBudget(Budget::PathPoints)));
            }
            Lathe if self.points.len() < 2 => return Err(fail("points", ErrorCode::OutOfRange)),
            Tube if self.path.len() < 2 => return Err(fail("path", ErrorCode::OutOfRange)),
            _ => {}
        }
        Ok(match self.shape {
            Box => Geometry::Box {
                size: self.size.expect("box size was required").map(Size::get),
            },
            Sphere => Geometry::Sphere {
                r: self.r.expect("sphere radius was required").get(),
            },
            Cylinder => Geometry::Cylinder {
                r: self.r.expect("cylinder radius was required").get(),
                h: self.h.expect("cylinder height was required").get(),
            },
            Cone => Geometry::Cone {
                r: self.r.expect("cone radius was required").get(),
                top: self.r2.map_or(0.0, Length::get),
                h: self.h.expect("cone height was required").get(),
            },
            Torus => {
                let ring = self.r.expect("torus ring radius was required").get();
                let tube = self.r2.expect("torus tube radius was required").get();
                if tube == 0.0 || tube >= ring {
                    return Err(fail("r2", ErrorCode::OutOfRange));
                }
                Geometry::Torus { ring, tube }
            }
            Extrude => Geometry::Extrude {
                outline: &self.points,
                depth: self.depth.expect("extrusion depth was required").get(),
            },
            Lathe => {
                if self.points.iter().any(|p| p[0].get() < 0.0) {
                    return Err(fail("points", ErrorCode::OutOfRange));
                }
                Geometry::Lathe {
                    profile: &self.points,
                }
            }
            Tube => {
                let thickness = self.r2.expect("tube thickness was required").get();
                if thickness == 0.0 {
                    return Err(fail("r2", ErrorCode::OutOfRange));
                }
                Geometry::Tube {
                    path: &self.path,
                    thickness,
                }
            }
        })
    }
}
