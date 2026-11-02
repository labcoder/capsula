//! Immutable prepared primitives; painters never read records or perform layout.
use crate::{
    Handle, Text,
    geom::{Affine, Rect, Vec2},
    record::{Align, Rgb},
};
use std::sync::Arc;

/// Text shaping parameters shared with painting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    /// Logical-pixel font size.
    pub size: f32,
    /// Whether to use the bold face.
    pub bold: bool,
}
/// Host text shaping, supplied during preparation rather than painting.
pub trait TextMeasure {
    /// Returns width and height, wrapping at `max_width` when present.
    /// # Errors
    /// Returns `MeasureError` when the host cannot shape this text.
    fn measure(
        &mut self,
        text: &str,
        style: TextStyle,
        max_width: Option<f32>,
    ) -> Result<[f32; 2], MeasureError>;
}
/// Content-free text-shaping failure; host diagnostics must not contain drawing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("scene.TextMeasure")]
pub struct MeasureError;
/// A resolved RGB color and its independent primitive opacity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Paint {
    /// RGB channels.
    pub rgb: Rgb,
    /// Opacity before object and sampled opacity are applied.
    pub opacity: f32,
}
impl From<Rgb> for Paint {
    fn from(rgb: Rgb) -> Self {
        Self { rgb, opacity: 1.0 }
    }
}
impl Paint {
    /// Multiplies this paint's opacity without changing its RGB channels.
    #[must_use]
    pub fn faded(self, opacity: f32) -> Self {
        Self {
            opacity: self.opacity * opacity,
            ..self
        }
    }
}
/// A resolved renderer-independent primitive.
#[derive(Debug, Clone, PartialEq)]
pub enum Prim {
    /// Native rounded rectangle, without flattened path storage.
    Rect {
        /// Geometry.
        rect: Rect,
        /// Corner radius.
        radius: f32,
        /// Optional fill.
        fill: Option<Paint>,
        /// Optional border color and width.
        border: Option<(Paint, f32)>,
    },
    /// Filled polygon with immutable vertex storage.
    Fill {
        /// Polygon vertices.
        points: Arc<[Vec2]>,
        /// Fill color.
        color: Paint,
    },
    /// Stroked polyline with immutable vertex storage.
    Line {
        /// Polyline vertices.
        points: Arc<[Vec2]>,
        /// Whether the last vertex connects to the first.
        closed: bool,
        /// Stroke color.
        color: Paint,
        /// Stroke width.
        width: f32,
        /// Whether to paint a dashed stroke.
        dashed: bool,
    },
    /// Text laid out using the host's matching text measurement.
    Text {
        /// Top-left point.
        origin: Vec2,
        /// Display content, redacted by Debug.
        text: Text,
        /// Shaping parameters.
        style: TextStyle,
        /// Foreground color.
        color: Paint,
        /// Optional wrapping width.
        max_width: Option<f32>,
        /// Alignment within the wrapping width.
        align: Align,
    },
}
/// Prepared geometry belonging to one deterministic object identity.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// Object identity.
    pub handle: Handle,
    /// Transform parent, if retained.
    pub parent: Option<Handle>,
    /// Shared geometry, never rebuilt by the painter.
    pub primitives: Arc<[Prim]>,
    /// Prepared bounds before sampled motion.
    pub bounds: Rect,
    /// Static transform inherited from detached ancestors.
    pub transform: Affine,
    /// Base opacity.
    pub opacity: f32,
}
/// Prepared object replacements for one step.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DisplayList {
    /// Items in stable paint order.
    pub items: Vec<Item>,
}
/// A sampled object referencing its immutable prepared geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameItem {
    /// Object identity.
    pub handle: Handle,
    /// Shared prepared primitives.
    pub primitives: Arc<[Prim]>,
    /// Affine coefficients `[a, b, c, d, e, f]` for `[a c e; b d f]`.
    pub transform: [f32; 6],
    /// Sampled opacity.
    pub opacity: f32,
    /// Revealed fraction of stroked geometry.
    pub reveal: f32,
}
/// Reused sampling output; clearing it retains its item capacity.
#[derive(Debug, Default)]
pub struct Frame {
    /// Items in stable paint order.
    pub items: Vec<FrameItem>,
}
