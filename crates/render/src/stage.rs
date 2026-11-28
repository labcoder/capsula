//! The stage side: what a stage thread holds and reads. Nothing here names a scene or
//! presenter type (`cargo xtask check-core` holds that), so a [`BuildInput`] copy is the only
//! way geometry reaches it.

mod frame;
mod input;
mod mesh;

use std::fmt;

pub use frame::{FrameBuf, Picture, Record};
pub use input::{Align, BuildInput, Object, Shape, Span};
pub use mesh::{Mesh, Piece, Vertex};

/// One staged view's geometry, numbered by the stager that staged it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeometryId(pub(crate) u64);

/// A resolved colour: packed RGB and its own opacity.
#[derive(Clone, Copy, PartialEq)]
pub struct Color {
    /// Packed `0xRRGGBB`.
    pub rgb: u32,
    /// Opacity before the object's.
    pub opacity: f32,
}

/// Drawing colours are content: only that there is one.
impl fmt::Debug for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Color(..)")
    }
}

/// Why a picture can't be drawn: a fixed kind with no content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// The cache isn't this renderer's, or belongs to another view than the frame's.
    #[error("a drawing's render cache belongs to another view")]
    Stale,
    /// The geometry doesn't fit its reserved buffers, or lyon refused it.
    #[error("a drawing's geometry doesn't fit its reserved buffers")]
    Build,
}

/// Appends `value` while `buffer` holds fewer than `cap`, so a reserved buffer never grows.
fn push<T>(buffer: &mut Vec<T>, cap: usize, value: T) -> Result<(), Failure> {
    if buffer.len() >= cap {
        return Err(Failure::Build);
    }
    buffer.push(value);
    Ok(())
}

/// A buffer position as the `u32` the stage side stores.
pub(crate) fn index(value: usize) -> Result<u32, Failure> {
    u32::try_from(value).map_err(|_| Failure::Build)
}
