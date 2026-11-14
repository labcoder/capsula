use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use super::super::{
    Scratch, paint_cache, paint_item,
    paint_phase::{self, Phase},
};
use crate::{allocation, presenter::Frames};
use gpui_kit::{
    Bounds, Context, DevicePixels, Font, FontId, FontMetrics, FontRun, GlyphId, IntoElement,
    LineLayout, NoopTextSystem, Pixels, PlatformTextSystem, Render, RenderGlyphParams,
    Result as GpuiResult, Size, Styled as _, TestAppContext, TextRenderingMode, Window, canvas, px,
    size,
};

pub(super) const HEADER_SIZE: f32 = 15.03125_f32 * 0.9_f32;
pub(super) const SYNTHETIC_GLYPH: GlyphId = GlyphId(0xFFFF_FF00);

pub(super) enum RefusedGlyph {
    Always(Arc<AtomicUsize>),
    Selective(Arc<Target>),
}

#[derive(Default)]
pub(super) struct Target {
    armed: AtomicBool,
    shapes: AtomicUsize,
    attempts: AtomicUsize,
    refusals: AtomicUsize,
    unexpected: AtomicUsize,
    key: [AtomicU64; 9],
}

impl Target {
    pub(super) fn counts(&self) -> [usize; 4] {
        [
            &self.shapes,
            &self.attempts,
            &self.refusals,
            &self.unexpected,
        ]
        .map(|count| count.load(Ordering::SeqCst))
    }

    pub(super) fn key(&self) -> [u64; 9] {
        std::array::from_fn(|index| self.key[index].load(Ordering::SeqCst))
    }

    pub(super) fn arm(&self) -> Armed<'_> {
        assert_eq!(
            self.counts(),
            [0; 4],
            "both target caches must still be cold"
        );
        assert!(!self.armed.swap(true, Ordering::SeqCst));
        Armed(self)
    }
}

pub(super) struct Armed<'a>(&'a Target);

impl Drop for Armed<'_> {
    fn drop(&mut self) {
        self.0.armed.store(false, Ordering::SeqCst);
    }
}

impl PlatformTextSystem for RefusedGlyph {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> GpuiResult<()> {
        NoopTextSystem.add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        NoopTextSystem.all_font_names()
    }

    fn font_id(&self, descriptor: &Font) -> GpuiResult<FontId> {
        NoopTextSystem.font_id(descriptor)
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        NoopTextSystem.font_metrics(font_id)
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> GpuiResult<Bounds<f32>> {
        NoopTextSystem.typographic_bounds(font_id, glyph_id)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> GpuiResult<Size<f32>> {
        NoopTextSystem.advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        NoopTextSystem.glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> GpuiResult<Bounds<DevicePixels>> {
        match self {
            Self::Always(refusals) => {
                refusals.fetch_add(1, Ordering::Relaxed);
            }
            Self::Selective(target) if params.glyph_id == SYNTHETIC_GLYPH => {
                target.attempts.fetch_add(1, Ordering::SeqCst);
                let key = [
                    u64::try_from(params.font_id.0).expect("the fixture font id fits u64"),
                    u64::from(params.glyph_id.0),
                    u64::from(f32::from(params.font_size).to_bits()),
                    u64::from(params.subpixel_variant.x),
                    u64::from(params.subpixel_variant.y),
                    u64::from(params.scale_factor.to_bits()),
                    u64::from(params.is_emoji),
                    u64::from(params.subpixel_rendering),
                    u64::from(params.dilation),
                ];
                for (slot, value) in target.key.iter().zip(key) {
                    slot.store(value, Ordering::SeqCst);
                }
                if target.armed.load(Ordering::SeqCst) {
                    target.refusals.fetch_add(1, Ordering::SeqCst);
                } else {
                    target.unexpected.fetch_add(1, Ordering::SeqCst);
                }
            }
            Self::Selective(_) => return NoopTextSystem.glyph_raster_bounds(params),
        }
        Err(std::io::Error::other("synthetic glyph refusal").into())
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> GpuiResult<(Size<DevicePixels>, Vec<u8>)> {
        if let Self::Selective(target) = self
            && params.glyph_id == SYNTHETIC_GLYPH
        {
            target.unexpected.fetch_add(1, Ordering::SeqCst);
            return Err(std::io::Error::other("unexpected synthetic glyph rasterization").into());
        }
        NoopTextSystem.rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let mut line = NoopTextSystem.layout_line(text, font_size, runs);
        if let Self::Selective(target) = self
            && text == "A"
            && f32::from(font_size).to_bits() == HEADER_SIZE.to_bits()
        {
            target.shapes.fetch_add(1, Ordering::SeqCst);
            if target.armed.load(Ordering::SeqCst) {
                assert_eq!(line.runs.len(), 1);
                assert_eq!(line.runs[0].glyphs.len(), 1);
                assert!(!line.runs[0].glyphs[0].is_emoji);
                assert_eq!(line.runs[0].glyphs[0].id, GlyphId(1));
                line.runs[0].glyphs[0].id = SYNTHETIC_GLYPH;
            }
        }
        line
    }

    fn recommended_rendering_mode(&self, font_id: FontId, font_size: Pixels) -> TextRenderingMode {
        NoopTextSystem.recommended_rendering_mode(font_id, font_size)
    }
}

#[test]
fn the_selective_target_requires_cold_shape_and_rejects_disarmed_synthetic_use() {
    let target = Arc::new(Target::default());
    let backend = RefusedGlyph::Selective(Arc::clone(&target));
    let ordinary = backend.layout_line("B", px(HEADER_SIZE), &[]);
    assert_eq!(ordinary.runs[0].glyphs[0].id, GlyphId(1));
    assert_eq!(target.counts(), [0; 4]);
    let params = RenderGlyphParams {
        font_id: FontId(0),
        glyph_id: SYNTHETIC_GLYPH,
        font_size: px(HEADER_SIZE),
        subpixel_variant: gpui_kit::point(2, 0),
        scale_factor: 1.0,
        is_emoji: false,
        subpixel_rendering: false,
        dilation: 0,
    };
    let ordinary_params = RenderGlyphParams {
        glyph_id: GlyphId(1),
        ..params
    };
    assert_eq!(
        backend
            .glyph_raster_bounds(&ordinary_params)
            .expect("ordinary glyph delegates"),
        Bounds::default()
    );
    {
        let _armed = target.arm();
        let target_line = backend.layout_line("A", px(HEADER_SIZE), &[]);
        assert_eq!(target_line.runs[0].glyphs[0].id, SYNTHETIC_GLYPH);
        assert_eq!(target_line.width, ordinary.width);
        assert!(backend.glyph_raster_bounds(&params).is_err());
    }
    assert_eq!(target.counts(), [1, 1, 1, 0]);
    assert_eq!(
        target.key(),
        [
            0,
            0xFFFF_FF00,
            u64::from(HEADER_SIZE.to_bits()),
            2,
            0,
            1_065_353_216,
            0,
            0,
            0
        ]
    );
    assert!(backend.glyph_raster_bounds(&params).is_err());
    assert_eq!(target.counts(), [1, 2, 1, 1]);
    let warmed = Arc::new(Target::default());
    let warmed_line =
        RefusedGlyph::Selective(Arc::clone(&warmed)).layout_line("A", px(HEADER_SIZE), &[]);
    assert_eq!(warmed_line.runs[0].glyphs[0].id, GlyphId(1));
    assert_eq!(warmed.counts(), [1, 0, 0, 0]);
    assert!(std::panic::catch_unwind(|| warmed.arm()).is_err());
    let warm = Target::default();
    assert!(
        std::panic::catch_unwind(|| {
            let _armed = warm.arm();
            panic!("body failure must disarm the target");
        })
        .is_err()
    );
    assert!(!warm.armed.load(Ordering::SeqCst));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Observed {
    result: Result<(), super::BuildError>,
    pending: [usize; 3],
    failed: bool,
}

struct Probe {
    frames: Rc<RefCell<Frames>>,
    scratch: Rc<RefCell<Scratch>>,
    armed: Rc<Cell<bool>>,
    observed: Rc<Cell<Option<Observed>>>,
}

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        let frames = Rc::clone(&self.frames);
        let scratch = Rc::clone(&self.scratch);
        let armed = Rc::clone(&self.armed);
        let observed = Rc::clone(&self.observed);
        canvas(
            |_, _, _| (),
            move |_, (), window, cx| {
                if !armed.get() {
                    return;
                }
                let _app = paint_phase::enter(Phase::App);
                let mut outcome = None;
                let failed = frames.borrow().paint(|frame, cache| {
                    let scene = paint_cache::bound(
                        cache,
                        frame.generation().ok_or(super::BuildError::Stale)?,
                    )?;
                    let capacity = scene.pending.capacity();
                    let result = frame.paint_items().try_for_each(|(item, first, prims)| {
                        paint_item(
                            item,
                            first,
                            prims,
                            scene,
                            &mut scratch.borrow_mut(),
                            window,
                            cx,
                        )
                    });
                    outcome = Some((
                        result,
                        [capacity, scene.pending.len(), scene.pending.capacity()],
                    ));
                    allocation::checkpoint(6, 1);
                    result
                });
                if let Some((result, pending)) = outcome {
                    observed.set(Some(Observed {
                        result,
                        pending,
                        failed,
                    }));
                }
            },
        )
        .size_full()
    }
}

#[gpui::test]
fn a_table_label_failure_keeps_its_quad_and_frees_the_unsubmitted_line(cx: &mut TestAppContext) {
    let refusals = Arc::new(AtomicUsize::new(0));
    let mut failing = TestAppContext::build_with_text_system(
        cx.dispatcher.clone(),
        Some("table label drain"),
        Arc::new(RefusedGlyph::Always(Arc::clone(&refusals))),
    );
    failing.update(gpui_kit::init);
    let scratch = Rc::new(RefCell::new(Scratch::new()));
    let (presenter, scene_id) = crate::presenter::tests::table_painter_fixture(Box::new(
        paint_cache::Stager::new(&scratch),
    ));
    let frames = presenter.frames();
    frames.borrow_mut().shown = true;
    let armed = Rc::new(Cell::new(false));
    let observed = Rc::new(Cell::new(None));
    let window = failing.open_window(size(px(100.0), px(100.0)), |_, _| Probe {
        frames: Rc::clone(&frames),
        scratch: Rc::clone(&scratch),
        armed: Rc::clone(&armed),
        observed: Rc::clone(&observed),
    });
    let mut visual = gpui_kit::VisualTestContext::from_window(window.into(), &failing);
    visual.run_until_parked();
    assert_eq!(observed.get(), None);
    assert_eq!(refusals.load(Ordering::Relaxed), 0);
    armed.set(true);
    let ((), events) = allocation::requests(16_384, || {
        visual.update(|window, cx| window.draw(cx).clear(cx));
    });
    assert_eq!(
        observed.get(),
        Some(Observed {
            result: Err(super::BuildError::Label),
            pending: [4, 0, 4],
            failed: true,
        })
    );
    assert_eq!(refusals.load(Ordering::Relaxed), 1);
    visual.update(|window, _| assert_eq!(window.painted_quads().len(), 1));
    assert_eq!(
        crate::presenter::tests::painted(&frames.borrow(), scene_id),
        None
    );
    let handoffs: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.action == 4)
        .collect();
    assert_eq!(handoffs.len(), 1);
    let (handed, handoff) = handoffs[0];
    assert_eq!((handoff.size, handoff.tag), (192, 1));
    let requested = events[..handed]
        .iter()
        .rfind(|event| event.action == 0 && event.pointer == handoff.pointer)
        .expect("the actual line handoff was allocated before it was marked");
    assert_eq!(
        (requested.size, requested.align, requested.tag),
        (192, align_of::<gpui_kit::PathVertex<Pixels>>(), 1)
    );
    let returned: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.action == 6)
        .collect();
    assert_eq!(returned.len(), 1);
    assert!(handed < returned[0].0);
    let frees: Vec<_> = events[handed + 1..returned[0].0]
        .iter()
        .filter(|event| event.action == 1 && event.pointer == handoff.pointer)
        .collect();
    assert_eq!(
        frees.len(),
        1,
        "the remaining line dies before the painter returns"
    );
    assert_eq!(
        (frees[0].size, frees[0].align, frees[0].tag),
        (192, handoff.align, 1)
    );
    visual.update(|window, _| window.remove_window());
    drop(visual);
    failing.quit();
}

struct RetainedLabel {
    cache: Rc<RefCell<paint_cache::PaintScene>>,
    item: Rc<RefCell<kumochi_scene::FrameItem>>,
    prims: Arc<[kumochi_scene::display::Prim]>,
    scratch: Rc<RefCell<Scratch>>,
}
impl Render for RetainedLabel {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        let cache = Rc::clone(&self.cache);
        let item = Rc::clone(&self.item);
        let prims = Arc::clone(&self.prims);
        let scratch = Rc::clone(&self.scratch);
        canvas(
            |_, _, _| (),
            move |_, (), window, cx| {
                let _app = paint_phase::enter(Phase::App);
                allocation::checkpoint(5, 2);
                paint_item(
                    &item.borrow(),
                    0,
                    &prims,
                    &mut cache.borrow_mut(),
                    &mut scratch.borrow_mut(),
                    window,
                    cx,
                )
                .expect("a retained label paints");
            },
        )
        .size_full()
    }
}
#[gpui::test]
fn warm_label_color_and_opacity_changes_reuse_layouts_across_hosts(cx: &mut TestAppContext) {
    use kumochi_scene::{
        display::{Paint, Prim, TextStyle},
        geom::Vec2,
        record::{Align, Rgb},
        text::Text,
    };
    cx.update(gpui_kit::init);
    for (text, wrap) in [("ab", None), ("abcdefghijklmnopqrstuvwxyz", Some(60.0))] {
        let prims: Arc<[Prim]> = Arc::from([Prim::Text {
            origin: Vec2::new(10.0, 10.0),
            text: Text::try_from(text.to_owned()).expect("fixture text"),
            style: TextStyle {
                size: 10.0,
                bold: false,
            },
            color: Paint::from(Rgb::try_from(0xff_0000).expect("RGB")),
            max_width: wrap,
            align: Align::Center,
        }]);
        let item = Rc::new(RefCell::new(super::item()));
        let cache = Rc::new(RefCell::new(paint_cache::PaintScene::of(
            item.borrow().handle,
            &prims,
        )));
        let scratch = Rc::new(RefCell::new(Scratch::new()));
        for _ in 0..2 {
            let root = cx.open_window(size(px(200.0), px(200.0)), |_, _| RetainedLabel {
                cache: Rc::clone(&cache),
                item: Rc::clone(&item),
                prims: Arc::clone(&prims),
                scratch: Rc::clone(&scratch),
            });
            let mut visual = gpui_kit::VisualTestContext::from_window(root.into(), cx);
            visual.run_until_parked();
            assert!(cache.borrow().labels[0].shaped.is_some());
            for (over, opacity) in [(None, 1.0), (Some(0x00_ff00), 0.5), (Some(0x00_00ff), 0.75)] {
                item.borrow_mut().color =
                    over.map(|rgb| Paint::from(Rgb::try_from(rgb).expect("RGB")));
                item.borrow_mut().opacity = opacity;
                let ((), events) = allocation::requests(16_384, || {
                    root.update(&mut visual, |_, _, cx| cx.notify())
                        .expect("owned root");
                });
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| (event.action, event.tag) == (5, 2))
                        .count(),
                    1
                );
                assert_eq!(paint_phase::tests::assert_paths(&events, 3), 0);
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| event.tag == 4 && matches!(event.action, 0 | 1))
                        .count(),
                    0
                );
            }
            visual.update(|window, _| window.remove_window());
        }
        if wrap == Some(60.0) {
            let cache = cache.borrow();
            let line = &cache.labels[0]
                .shaped
                .as_ref()
                .expect("retained layout")
                .lines[0];
            for (align, expected) in [
                (gpui_kit::TextAlign::Left, 10.0),
                (gpui_kit::TextAlign::Center, 22.0),
                (gpui_kit::TextAlign::Right, 34.0),
            ] {
                assert_eq!(
                    super::super::row_origin(line, px(10.0), px(60.0), px(120.0), align, None),
                    px(expected)
                );
            }
        }
    }
}
