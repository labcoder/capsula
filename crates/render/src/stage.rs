//! The stage side: what a stage thread holds and reads. A [`BuildInput`] batch is the only way
//! geometry reaches it. `cargo xtask check-core` lints these files for paths to the scene,
//! the presenter or the rest of the crate; it reads spellings, so it is a lint, not a proof.

mod frame;
mod gpu;
mod input;
mod mesh;

use std::fmt;

pub use frame::{FrameBuf, Picture, Record};
pub use gpu::{Built, Gpu, GpuFailure};
pub use input::{Align, BuildInput, Object, Part, Shape, Span};
#[cfg(test)]
pub(crate) use mesh::earliest;
pub use mesh::{Mesh, Piece, Vertex};

/// One staged view's geometry, numbered by the stager that staged it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeometryId(pub(crate) u64);

/// Where a view's copy stopped: the first primitive a batch didn't take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// The object's index in its geometry's sorted objects.
    pub object: u32,
    /// The primitive's index in that object.
    pub prim: u32,
}

impl Cursor {
    /// A view's first primitive.
    pub const START: Self = Self { object: 0, prim: 0 };
}

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
    /// A batch isn't the next part of the geometry the mesh is building, or a copy names a
    /// cursor no copy returned or a view its batch already holds.
    #[error("a drawing's batch arrived out of order")]
    Order,
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
