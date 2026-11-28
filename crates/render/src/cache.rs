//! The owner side's paint cache and stager. Staging collects a view's shared sources and
//! sorts them by handle; it builds and sends nothing, and the sources never leave the owner.

use std::{any::Any, cell::Cell, fmt, sync::Arc};

use kumochi_scene::{Handle, SceneView, display::Prim};
use kumochi_shell_core::presenter::{SceneCache, Stage};

use crate::{
    limits::FIXED_BYTES,
    stage::{Failure, GeometryId},
};

/// One object's handle, the scale its rect corners were charged at, and its primitives.
pub(crate) type Source = (Handle, f32, Arc<[Prim]>);

/// Stages render caches for the presenter, numbering each staged geometry.
pub struct Stager {
    next: Cell<u64>,
}

impl Stager {
    /// A stager whose first geometry is numbered 0.
    pub fn new() -> Self {
        Self { next: Cell::new(0) }
    }
}

impl Default for Stager {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Stager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stager")
            .field("staged", &self.next.get())
            .finish()
    }
}

impl Stage for Stager {
    fn stage(&self, view: &SceneView) -> Box<dyn SceneCache> {
        let id = GeometryId(self.next.get());
        self.next.set(
            id.0.checked_add(1)
                .expect("a u64 geometry counter never runs out"),
        );
        let mut sources: Vec<Source> = view
            .shared_sources()
            .map(|(handle, scale, prims)| (handle, scale, Arc::clone(prims)))
            .collect();
        sources.sort_unstable_by_key(|source| source.0);
        Box::new(RenderCache::new(id, Arc::clone(view.generation()), sources))
    }

    fn fixed_bytes(&self) -> usize {
        FIXED_BYTES
    }
}

/// What one committed view's pictures are built and recorded from, bound to its generation.
pub struct RenderCache {
    id: GeometryId,
    generation: Arc<()>,
    /// Sorted by handle.
    sources: Box<[Source]>,
    bytes: usize,
}

impl RenderCache {
    fn new(id: GeometryId, generation: Arc<()>, sources: Vec<Source>) -> Self {
        let sources = sources.into_boxed_slice();
        let bytes = sources
            .len()
            .checked_mul(size_of::<Source>())
            .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
            .expect("a view holds at most MAX_LIVE_OBJECTS objects");
        Self {
            id,
            generation,
            sources,
            bytes,
        }
    }

    /// The geometry it was staged as.
    pub fn id(&self) -> GeometryId {
        self.id
    }

    pub(crate) fn sources(&self) -> &[Source] {
        &self.sources
    }

    /// The renderer's cache, staged from the view generation a frame is bound to.
    pub(crate) fn bound<'a>(
        cache: &'a dyn SceneCache,
        generation: &Arc<()>,
    ) -> Result<&'a Self, Failure> {
        let cache: &dyn Any = cache;
        cache
            .downcast_ref::<Self>()
            .filter(|cache| Arc::ptr_eq(generation, &cache.generation))
            .ok_or(Failure::Stale)
    }

    /// A cache of hand-made objects, for tests that need primitives no layout produces.
    #[cfg(test)]
    pub(crate) fn of(id: u64, mut sources: Vec<Source>) -> Self {
        sources.sort_unstable_by_key(|source| source.0);
        Self::new(GeometryId(id), Arc::new(()), sources)
    }
}

/// Counts only: the primitives are the drawing.
impl fmt::Debug for RenderCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderCache")
            .field("id", &self.id)
            .field("objects", &self.sources.len())
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}

impl SceneCache for RenderCache {
    fn bytes(&self) -> usize {
        self.bytes
    }
}
