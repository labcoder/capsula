//! How Kumo looks apart from its shape and expression: the material and the colours. Each is a
//! parameter set, never new art.

use super::math::{Rgb, Vec3, hex, v3};

/// How light falls on the body: a soft key light, sky fill, a faint sheen and a luminous rim.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Material {
    pub(crate) key_dir: Vec3,
    pub(crate) key: f32,
    pub(crate) ambient: f32,
    /// Diffuse wrap: higher reads softer, more like vapour than plastic.
    pub(crate) wrap: f32,
    pub(crate) sheen: f32,
    pub(crate) sheen_power: f32,
    /// How much inner light shows at the edge, like subsurface scattering.
    pub(crate) rim: f32,
    /// Light scattered through thin vapour near the silhouette: a silver lining.
    pub(crate) scatter: f32,
    /// The contact shadow's strength.
    pub(crate) shadow: f32,
}

impl Material {
    /// The approved material.
    pub(crate) const SOFT: Self = Self {
        key_dir: v3(-0.45, 0.70, 0.55),
        key: 0.52,
        ambient: 0.64,
        wrap: 0.6,
        sheen: 0.015,
        sheen_power: 3.0,
        rim: 0.45,
        scatter: 0.45,
        shadow: 0.22,
    };
}

/// Body colours, the accent that glows from one side, the eye ink and the shadow.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Theme {
    pub(crate) top: Rgb,
    pub(crate) bottom: Rgb,
    pub(crate) accent: Rgb,
    /// Where light doesn't reach: a deeper, saturated body colour, never grey.
    pub(crate) shade: Rgb,
    pub(crate) eye: Rgb,
    pub(crate) shadow: Rgb,
}

impl Theme {
    /// The approved colours.
    pub(crate) const LAVENDER: Self = Self {
        top: hex(0xFB_FA_FF),
        bottom: hex(0xB4_B0_F0),
        accent: hex(0x86_C9_FF),
        shade: hex(0x8C_86_E8),
        eye: hex(0x35_3A_5E),
        shadow: hex(0x6F_6A_A8),
    };
}

/// The material and colours together, and whether a contact shadow sits under the body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Look {
    pub(crate) material: Material,
    pub(crate) theme: Theme,
    pub(crate) shadow: bool,
}

impl Default for Look {
    fn default() -> Self {
        Self {
            material: Material::SOFT,
            theme: Theme::LAVENDER,
            shadow: true,
        }
    }
}
