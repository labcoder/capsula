//! Kumochi's drawing renderer: what a stage thread draws from. The owner side stages
//! a view's shared sources in a [`RenderCache`], copies them into a [`BuildInput`] and writes
//! each frame into a [`FrameBuf`]. The stage side builds a [`Mesh`] from that copy once per
//! geometry. Nothing on the stage side can name a scene type, so the copy and the records are
//! all that cross (docs/development/render.md).

mod cache;
mod input;
pub mod limits;
mod record;
mod stage;

pub use cache::{RenderCache, Stager};
pub use record::record;
pub use stage::{
    Align, BuildInput, Color, Cursor, Failure, FrameBuf, GeometryId, Mesh, Object, Part, Picture,
    Piece, Record, Shape, Span, Vertex,
};

#[cfg(test)]
mod tests;
