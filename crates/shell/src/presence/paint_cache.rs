//! The overlay's paint cache for one committed view: each `Prim::Fill` is tessellated once, in
//! prim space, when the view is staged, so a paint only places its triangles. The presenter
//! owns the cache in the view's frame slot and charges its bytes with the scene. Labels own
//! their shared text once and retain GPUI layouts until the font/size/wrap/scale key changes.

use std::{any::Any, cell::RefCell, fmt, rc::Rc, sync::Arc};

use gpui_kit::{
    Font, FontId, FontWeight, PaintQuad, Path, Pixels, PlatformTextSystem, SharedString, TextRun,
    WindowTextSystem, WrappedLine, transparent_black,
};
use kumochi_scene::{
    Handle, SceneView,
    display::{Paint, Prim},
    geom::{Affine, Vec2},
};
use smallvec::SmallVec;

use super::paint_paths::{self, BuildError};
use super::paint_stroke::Scratch;
use crate::presenter::{SceneCache, Stage, drawing_font, resolve_font};

/// Stages the overlay's paint caches for the presenter.
pub(super) struct Stager(pub(super) usize);

impl Stager {
    pub(super) fn new(scratch: &Rc<RefCell<Scratch>>) -> Self {
        Self(
            size_of::<Self>()
                + 2 * size_of::<usize>()
                + size_of::<RefCell<Scratch>>()
                + scratch.borrow().bytes(),
        )
    }
}

impl Stage for Stager {
    fn stage(&self, view: &SceneView) -> Box<dyn SceneCache> {
        Box::new(PaintScene::staged(view))
    }

    fn fixed_bytes(&self) -> usize {
        self.0
    }
}

/// What one committed view's frames paint from, bound to that view's generation.
pub(super) struct PaintScene {
    generation: Arc<()>,
    /// Requested bytes of this cache boxed, computed once when staged.
    bytes: usize,
    /// Sorted by handle, then primitive index.
    fills: Box<[FillEntry]>,
    pub(super) labels: Box<[LabelEntry]>,
    pub(super) pending: Vec<Pending>,
}

pub(super) enum Pending {
    Quad(PaintQuad),
    Path(Path<Pixels>, Paint),
    Text(usize),
}

const _: () = {
    const fn send<T: Send>() {}
    send::<PaintScene>();
};

struct FillEntry {
    handle: Handle,
    prim: usize,
    /// Triangle corners in prim space, or why the fill can't paint.
    triangles: Result<Box<[Vec2]>, BuildError>,
}

pub(super) const FONT_FEATURE_BYTES: usize =
    (2 * size_of::<usize>() + size_of::<Vec<(String, u32)>>()).next_multiple_of(8);

pub(super) struct Fonts([FontSlot; 2]);

struct FontSlot {
    font: Font,
    id: Option<FontId>,
    generation: u64,
}

impl Fonts {
    pub(super) fn new() -> Self {
        let regular = drawing_font(SharedString::default(), FontWeight::NORMAL);
        let bold = Font {
            weight: FontWeight::BOLD,
            ..regular.clone()
        };
        Self([regular, bold].map(|font| FontSlot {
            font,
            id: None,
            generation: 0,
        }))
    }

    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the scene label's bold flag, which picks the font slot"
    )]
    fn select(&mut self, family: &SharedString, bold: bool) -> &mut FontSlot {
        if &self.0[0].font.family != family {
            for slot in &mut self.0 {
                slot.font.family = family.clone();
                slot.id = None;
                slot.generation = slot
                    .generation
                    .checked_add(1)
                    .expect("font generation fits");
            }
        }
        &mut self.0[usize::from(bold)]
    }
}

pub(super) struct LabelEntry {
    handle: Handle,
    pub(super) prim: usize,
    text: SharedString,
    text_bytes: usize,
    bold: bool,
    pub(super) shaped: Option<Shaped>,
}

pub(super) struct Shaped {
    font_gen: u64,
    size: Pixels,
    wrap: Option<Pixels>,
    scale: u32,
    pub(super) line_height: Pixels,
    pub(super) lines: SmallVec<[WrappedLine; 1]>,
}

impl LabelEntry {
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the scene label's bold flag, passed through"
    )]
    fn new(handle: Handle, prim: usize, text: &str, bold: bool) -> Self {
        let (text, text_bytes) = if text.len() > 23 {
            let owned = Arc::<str>::from(text);
            let shared = SharedString::from(Arc::clone(&owned));
            let bytes = if std::ptr::eq(owned.as_ptr(), shared.as_ptr()) {
                (2 * size_of::<usize>() + text.len()).next_multiple_of(8)
            } else {
                0
            };
            (shared, bytes)
        } else {
            (SharedString::new(text), 0)
        };
        Self {
            handle,
            prim,
            text,
            text_bytes,
            bold,
            shaped: None,
        }
    }

    pub(super) fn shape(
        &mut self,
        fonts: &mut Fonts,
        font: (&SharedString, Option<&dyn PlatformTextSystem>),
        shaper: &WindowTextSystem,
        size: Pixels,
        wrap: Option<Pixels>,
        scale: f32,
    ) -> Result<&Shaped, BuildError> {
        let (family, backend) = font;
        let slot = fonts.select(family, self.bold);
        if !self.shaped.as_ref().is_some_and(|shaped| {
            shaped.font_gen == slot.generation
                && shaped.size == size
                && shaped.wrap == wrap
                && shaped.scale == scale.to_bits()
        }) {
            self.shaped = None;
            let id = if let Some(id) = slot.id {
                id
            } else {
                let id =
                    resolve_font(shaper, backend, &slot.font).map_err(|_| BuildError::Label)?;
                slot.id = Some(id);
                id
            };
            let line_height = shaper.ascent(id, size).abs() + shaper.descent(id, size).abs();
            let runs = [TextRun {
                len: self.text.len(),
                font: slot.font.clone(),
                color: transparent_black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            let lines = shaper
                .shape_text(self.text.clone(), size, &runs, wrap, None)
                .map_err(|_| BuildError::Label)?;
            self.shaped = Some(Shaped {
                font_gen: slot.generation,
                size,
                wrap,
                scale: scale.to_bits(),
                line_height,
                lines,
            });
        }
        self.shaped.as_ref().ok_or(BuildError::Label)
    }
}

impl fmt::Debug for LabelEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LabelEntry")
            .field("len", &self.text.len())
            .field("shaped", &self.shaped)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Shaped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shaped")
            .field("lines", &self.lines.len())
            .field(
                "rows",
                &self
                    .lines
                    .iter()
                    .map(|line| line.wrap_boundaries.len() + 1)
                    .sum::<usize>(),
            )
            .finish_non_exhaustive()
    }
}

impl PaintScene {
    /// Tessellates every fill of the view's objects; a fill lyon or its limit refuses is kept
    /// as that refusal, so it fails when it paints, as it did when built per paint.
    pub(super) fn staged(view: &SceneView) -> Self {
        let capacity = view
            .paint_sources()
            .map(|(_, prims)| pending_capacity(prims))
            .max()
            .unwrap_or(0);
        let fills = view
            .paint_sources()
            .flat_map(|(handle, prims)| entries(handle, prims))
            .collect();
        let labels = view
            .paint_sources()
            .flat_map(|(handle, prims)| labels(handle, prims))
            .collect();
        Self::new(Arc::clone(view.generation()), fills, labels, capacity)
    }

    fn new(
        generation: Arc<()>,
        mut fills: Vec<FillEntry>,
        mut labels: Vec<LabelEntry>,
        capacity: usize,
    ) -> Self {
        fills.sort_unstable_by_key(|fill| (fill.handle, fill.prim));
        let fills = fills.into_boxed_slice();
        labels.sort_unstable_by_key(|label| (label.handle, label.prim));
        let labels = labels.into_boxed_slice();
        let corners: usize = fills
            .iter()
            .map(|fill| fill.triangles.as_deref().map_or(0, <[Vec2]>::len))
            .sum();
        let pending = Vec::with_capacity(capacity);
        let bytes = size_of::<Self>()
            + fills.len() * size_of::<FillEntry>()
            + corners * size_of::<Vec2>()
            + labels.len() * size_of::<LabelEntry>()
            + labels.iter().map(|label| label.text_bytes).sum::<usize>()
            + pending.capacity() * size_of::<Pending>();
        Self {
            generation,
            bytes,
            fills,
            labels,
            pending,
        }
    }

    /// The fill at primitive `prim` of `handle`, placed by `transform` into one exactly sized
    /// path; none when it covers nothing.
    ///
    /// # Errors
    /// The fill's staged refusal, or [`BuildError::Stale`] for a primitive this view never staged.
    pub(super) fn fill(
        &self,
        handle: Handle,
        prim: usize,
        transform: Affine,
    ) -> Result<Option<Path<Pixels>>, BuildError> {
        let index = self
            .fills
            .binary_search_by_key(&(handle, prim), |fill| (fill.handle, fill.prim))
            .map_err(|_| BuildError::Stale)?;
        let fill = &self.fills[index];
        let triangles = fill.triangles.as_deref().map_err(|error| *error)?;
        Ok(paint_paths::placed(triangles.iter().copied(), transform))
    }

    pub(super) fn label_index(&self, handle: Handle, prim: usize) -> Result<usize, BuildError> {
        self.labels
            .binary_search_by_key(&(handle, prim), |label| (label.handle, label.prim))
            .map_err(|_| BuildError::Stale)
    }

    /// A cache of one object's primitives, for tests that build an item by hand.
    #[cfg(test)]
    pub(super) fn of(handle: Handle, prims: &[Prim]) -> Self {
        Self::new(
            Arc::new(()),
            entries(handle, prims).collect(),
            labels(handle, prims).collect(),
            pending_capacity(prims),
        )
    }
}

/// Counts only: the corners are the drawing's geometry.
impl fmt::Debug for PaintScene {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaintScene")
            .field("fills", &self.fills.len())
            .field("bytes", &self.bytes)
            .field("labels", &self.labels)
            .finish_non_exhaustive()
    }
}

impl SceneCache for PaintScene {
    fn bytes(&self) -> usize {
        self.bytes
    }
}

/// The paint cache staged from the view generation a frame is bound to.
///
/// # Errors
/// [`BuildError::Stale`] for a cache staged from another view, or by another stage.
pub(super) fn bound<'a>(
    cache: &'a mut dyn SceneCache,
    generation: &Arc<()>,
) -> Result<&'a mut PaintScene, BuildError> {
    let cache: &mut dyn Any = cache;
    cache
        .downcast_mut::<PaintScene>()
        .filter(|scene| Arc::ptr_eq(generation, &scene.generation))
        .ok_or(BuildError::Stale)
}

fn pending_capacity(prims: &[Prim]) -> usize {
    prims
        .iter()
        .map(|prim| {
            if matches!(prim, Prim::Rect { .. }) {
                2
            } else {
                1
            }
        })
        .sum()
}

fn entries(handle: Handle, prims: &[Prim]) -> impl Iterator<Item = FillEntry> {
    prims
        .iter()
        .enumerate()
        .filter_map(move |(prim, each)| match each {
            Prim::Fill { points, .. } => Some(FillEntry {
                handle,
                prim,
                triangles: paint_paths::fill_triangles(points),
            }),
            _ => None,
        })
}

fn labels(handle: Handle, prims: &[Prim]) -> impl Iterator<Item = LabelEntry> {
    prims
        .iter()
        .enumerate()
        .filter_map(move |(prim, each)| match each {
            Prim::Text { text, style, .. } => {
                Some(LabelEntry::new(handle, prim, text.as_str(), style.bold))
            }
            _ => None,
        })
}

#[cfg(test)]
#[path = "paint_cache_tests.rs"]
mod tests;
