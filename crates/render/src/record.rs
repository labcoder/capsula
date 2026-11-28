//! Writing one frame into a frame buffer on the owner thread: arithmetic into reserved
//! buffers and a binary search per item, so it never allocates, waits or locks.

use kumochi_protocol::message::SceneId;
use kumochi_scene::Frame;
use kumochi_shell_core::presenter::SceneCache;

use crate::{
    cache::RenderCache,
    input::color,
    stage::{Failure, FrameBuf, Record, index},
};

/// Lists `frame` in `buf` as a picture of `scene`: one record per paint item, in
/// [`Frame::paint_items`] order, so every object's geometry before any edge label. Returns
/// whether it listed a picture; a frame with nothing to paint is left out.
///
/// # Errors
/// [`Failure::Stale`] for a cache that isn't this renderer's, was staged from another view, or
/// lacks an item's object; [`Failure::Build`] past the reserved pictures or records. `buf` is
/// then as it was.
pub fn record(
    buf: &mut FrameBuf,
    scene: SceneId,
    frame: &Frame,
    cache: &mut dyn SceneCache,
) -> Result<bool, Failure> {
    let Some(generation) = frame.generation() else {
        return Ok(false);
    };
    let cache = RenderCache::bound(cache, generation)?;
    let sources = cache.sources();
    buf.picture(scene, cache.id(), |records| {
        for (item, first, prims) in frame.paint_items() {
            let object = sources
                .binary_search_by_key(&item.handle, |source| source.0)
                .map_err(|_| Failure::Stale)?;
            records.push(Record {
                object: index(object)?,
                first: index(first)?,
                count: index(prims.len())?,
                transform: item.transform.0,
                opacity: item.opacity,
                draw: item.draw,
                color: item.color.map(color),
                flow: item.flow,
                paint_scale: item.paint_scale,
            })?;
        }
        Ok(())
    })
}
