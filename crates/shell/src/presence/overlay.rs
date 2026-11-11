//! The drawing overlay: one transparent, click-through window over the primary display that
//! paints the presenter's sampled frames, shown only while one has something to paint.
//! Soft-row painting adapts gpui-pre 0.3.10's line.rs for retained geometry and current ink;
//! its Apache-2.0 attribution and adaptation details are in THIRD-PARTY-NOTICES.md.
// shortcut: primary display only; add per-display windows once a second display can be certified

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Instant,
};

use async_channel::Sender;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, BorderStyle, Bounds, Context, DecorationRun, Element,
    ElementId, Entity, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, PaintQuad,
    Pixels, Point, Refineable as _, Render, Rgba, Style, StyleRefinement, Styled, TextAlign,
    WeakEntity, Window, WrapBoundary, WrappedLine, point, px, quad, rgb, size, transparent_black,
};
use kumochi_protocol::message::{ControlToBrain, MAX_VISIBLE_SCENES, SceneId};
use kumochi_scene::{
    FrameItem,
    display::{Paint, Prim},
    geom::{Affine, Rect, Vec2},
    record::Align,
};

use super::overlay_visibility::Surface;
use super::paint_cache::{self, PaintScene, Pending};
use super::paint_geometry;
use super::paint_paths::{self, BuildError};
use super::paint_phase::{self, Phase};
use super::paint_stroke::Scratch;
use super::{OpenError, Presence, configure_surface, options, scaled};
use crate::platform::{self, PresenceEvent, PresenceSurface};
use crate::presenter::{Frames, Sample};

/// One overlay window and its root, opened hidden and closed once hidden again.
pub(super) struct Overlay {
    pub(super) window: AnyWindowHandle,
    drawing: Entity<Drawing>,
    painting: Rc<Cell<bool>>,
}

#[derive(Default)]
pub(super) struct PaintNotice {
    pending: Cell<bool>,
    failed: Cell<bool>,
    seen: Cell<[Option<(SceneId, u64, u32)>; MAX_VISIBLE_SCENES]>,
}

impl PaintNotice {
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "whether this paint failed, folded into the one pending notice"
    )]
    fn observe(
        &self,
        failed: bool,
        observations: [Option<(SceneId, u64, u32)>; MAX_VISIBLE_SCENES],
    ) -> bool {
        self.failed.set(self.failed.get() || failed);
        let before = self.seen.replace(observations);
        let fresh = observations
            .iter()
            .flatten()
            .any(|observation| !before.contains(&Some(*observation)));
        (failed || fresh) && !self.pending.replace(true)
    }
}

#[cfg(test)]
impl Overlay {
    pub(super) fn detached(
        frames: Rc<RefCell<Frames>>,
        owner: WeakEntity<Presence>,
        scratch: Rc<RefCell<Scratch>>,
        notice: Rc<PaintNotice>,
        cx: &mut App,
    ) -> Self {
        let painting = Rc::new(Cell::new(false));
        let gate = Rc::clone(&painting);
        let (window, drawing) = gpui_kit::open_window(options(), cx, |_, cx| {
            cx.new(|_| Drawing {
                frames,
                owner,
                painting: gate,
                scratch,
                notice,
            })
        })
        .expect("a hidden GPUI test overlay");
        Self {
            window,
            drawing,
            painting,
        }
    }
}

/// The overlay's root: every sampled frame, in scene order.
struct Drawing {
    frames: Rc<RefCell<Frames>>,
    owner: WeakEntity<Presence>,
    // A retired root can still have a queued paint after another window shows.
    painting: Rc<Cell<bool>>,
    scratch: Rc<RefCell<Scratch>>,
    notice: Rc<PaintNotice>,
}

impl Render for Drawing {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        let _app = paint_phase::enter(Phase::App);
        DrawingElement {
            frames: Rc::clone(&self.frames),
            owner: self.owner.clone(),
            painting: Rc::clone(&self.painting),
            scratch: Rc::clone(&self.scratch),
            notice: Rc::clone(&self.notice),
            style: StyleRefinement::default(),
            #[cfg(test)]
            observation: None,
        }
        .size_full()
    }
}

struct DrawingElement {
    frames: Rc<RefCell<Frames>>,
    owner: WeakEntity<Presence>,
    painting: Rc<Cell<bool>>,
    scratch: Rc<RefCell<Scratch>>,
    notice: Rc<PaintNotice>,
    style: StyleRefinement,
    #[cfg(test)]
    observation: Option<u8>,
}

impl IntoElement for DrawingElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Styled for DrawingElement {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl Element for DrawingElement {
    type RequestLayoutState = Style;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Style) {
        let _app = paint_phase::enter(Phase::App);
        #[cfg(test)]
        {
            self.observation = tests::recovery::draw_enter(
                window.window_handle().window_id().as_u64(),
                self.painting.get(),
            );
        }
        let mut style = Style::default();
        style.refine(&self.style);
        let layout_style = style.clone();
        let id = {
            let _gpui = paint_phase::enter(Phase::Gpui);
            window.request_layout(layout_style, [], cx)
        };
        (id, style)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Style,
        _: &mut Window,
        _: &mut App,
    ) {
        let _app = paint_phase::enter(Phase::App);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        style: &mut Style,
        (): &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let _app = paint_phase::enter(Phase::App);
        #[cfg(test)]
        if self.observation.is_none() {
            self.observation = tests::recovery::draw_enter(
                window.window_handle().window_id().as_u64(),
                self.painting.get(),
            );
        }
        let _gpui = paint_phase::enter(Phase::Gpui);
        style.paint(bounds, window, cx, |window, cx| {
            let _app = paint_phase::enter(Phase::App);
            #[cfg(test)]
            crate::allocation::checkpoint(5, 1);
            let failed = self.painting.get()
                && self.frames.borrow().paint(|frame, cache| {
                    // An unsampled frame paints nothing and has no view to check against.
                    let Some(generation) = frame.generation() else {
                        return Ok(());
                    };
                    let scene = paint_cache::bound(cache, generation)?;
                    frame.paint_items().try_for_each(|(item, first, prims)| {
                        paint_item(
                            item,
                            first,
                            prims,
                            scene,
                            &mut self.scratch.borrow_mut(),
                            window,
                            cx,
                        )
                    })
                });
            let observations = if self.painting.get() {
                self.frames.borrow().observations()
            } else {
                [None; MAX_VISIBLE_SCENES]
            };
            if self.painting.get() && self.notice.observe(failed, observations) {
                let owner = self.owner.clone();
                let notice = Rc::clone(&self.notice);
                let notify = move |cx: &mut App| {
                    let _app = paint_phase::enter(Phase::App);
                    notice.pending.set(false);
                    let failed = notice.failed.replace(false);
                    let result = {
                        let _gpui = paint_phase::enter(Phase::Gpui);
                        owner.update(cx, |this, cx| this.paint_observed(failed, cx))
                    };
                    if let Ok(Err(error)) = result {
                        tracing::error!(%error, "can't follow a drawing paint");
                    }
                };
                cx.defer(notify);
            }
        });
        #[cfg(test)]
        tests::recovery::draw_return(self.observation.take());
    }
}

impl Presence {
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "whether the paint failed, as observe takes it"
    )]
    fn paint_observed(
        &mut self,
        failed: bool,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let _app = paint_phase::enter(Phase::App);
        #[cfg(test)]
        if failed {
            tests::recovery::apply_enter();
        }
        let effects = self.presenter.paint_failed();
        #[cfg(test)]
        if failed {
            tests::recovery::intent(&effects);
        }
        #[cfg(not(test))]
        let _ = failed;
        if !effects.is_empty() {
            self.scene_effects(Ok(effects), cx);
        }
        if matches!(
            self.overlay,
            super::overlay_visibility::Visibility::Shown(_)
        ) && self
            .overlay
            .window()
            .is_some_and(|host| host.painting.get())
            && let Some(session) = &self.session
        {
            let generation = session.generation();
            for shown in self.presenter.take_shown(generation).into_iter().flatten() {
                let sent = self.session.as_ref().is_some_and(|session| {
                    session.generation() == shown.generation
                        && self.link.send_control(
                            session,
                            ControlToBrain::StepShown {
                                run: shown.run,
                                scene: shown.scene,
                                index: shown.index,
                            },
                        )
                });
                if !sent {
                    self.end_scene_connection();
                    self.draw_changed(cx);
                    break;
                }
            }
        }
        let result = self.runs_changed(cx);
        #[cfg(test)]
        if failed && result.is_ok() {
            tests::recovery::apply_return();
        }
        result
    }

    /// Follows a change in the mirrored scenes: samples them, then shows the overlay while a
    /// frame has something to paint and hides it otherwise. At rest it asks for no frames.
    pub(super) fn draw_changed(&mut self, cx: &mut Context<'_, Self>) {
        if self.draw_changed_at(Instant::now(), cx) {
            self.want_frames(cx);
        }
    }

    pub(super) fn draw_changed_at(&mut self, now: Instant, cx: &mut Context<'_, Self>) -> bool {
        if let Some(session) = &self.session {
            let effects = self.presenter.advance(session.generation(), now);
            match effects {
                Ok(effects) if effects.is_empty() => {}
                effects => {
                    return self.scene_effects_at(effects, now, cx);
                }
            }
        }
        let (sample, effects) = self.presenter.sample_at(now);
        if !effects.is_empty() {
            return self.scene_effects_at(Ok(effects), now, cx);
        }
        if let Err(error) = self.sync_overlay(sample, cx) {
            tracing::error!(%error, "the drawing overlay couldn't update");
            // A drawing that can't be shown is cleared rather than kept off screen.
            if sample.visible {
                let effects = self.presenter.clear(now);
                return self.scene_effects_at(effects, now, cx);
            }
        }
        let moving = self.presenter.animating(now);
        self.scene_watchdog(now, moving, cx);
        moving
    }

    pub(super) fn sync_overlay(
        &mut self,
        sample: Sample,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let frames = self.presenter.frames();
        if sample.visible && self.overlay.window().is_some() {
            self.protect_surface(super::exclusion::Slot::Drawing, cx)?;
        }
        let handles = self.exclusion_handles();
        let mut surface = Gpui {
            owner: cx.weak_entity(),
            exclusion: &mut self.exclusion,
            surfaces: super::exclusion::Windows {
                native: &self.native,
                handles,
                cx,
                #[cfg(test)]
                fixture: self.exclusion_fixture.clone(),
            },
            frames: &frames,
            events: &self.events,
            scratch: &self.paint_scratch,
            notice: &self.paint_notice,
        };
        self.overlay.sync(&mut surface, &frames, sample)
    }
}

/// The overlay's windows through GPUI's own open and close.
struct Gpui<'a> {
    exclusion: &'a mut super::exclusion::Exclusion<super::exclusion::Receipt>,
    surfaces: super::exclusion::Windows<'a>,
    owner: WeakEntity<Presence>,
    frames: &'a Rc<RefCell<Frames>>,
    events: &'a Sender<PresenceEvent>,
    scratch: &'a Rc<RefCell<Scratch>>,
    notice: &'a Rc<PaintNotice>,
}

impl Surface for Gpui<'_> {
    type Window = Overlay;

    fn open(&mut self) -> Result<Overlay, OpenError> {
        let owner = self.owner.clone();
        let events = self.events.clone();
        open(
            Rc::clone(self.frames),
            owner,
            Rc::clone(self.scratch),
            Rc::clone(self.notice),
            self.surfaces.cx,
            |window, cx| {
                #[cfg(test)]
                if let Some(result) = tests::recovery::native(tests::recovery::Operation::Configure)
                {
                    return result;
                }
                configure_surface(window, PresenceSurface::Overlay, events, cx)
            },
        )
    }

    fn show(&mut self, overlay: &Overlay) -> Result<(), OpenError> {
        #[expect(
            clippy::as_conversions,
            reason = "fixed semantic slot indexes the seven handles"
        )]
        let drawing = super::exclusion::Slot::Drawing as usize;
        self.surfaces.handles[drawing] = Some(overlay.window);
        self.surfaces
            .protect(self.exclusion, super::exclusion::Slot::Drawing)?;
        #[cfg(test)]
        if let Some(result) = tests::recovery::native(tests::recovery::Operation::Show) {
            return result.map_err(OpenError::from);
        }
        show(overlay.window, self.surfaces.cx)
    }

    fn repaint(&mut self, overlay: &Overlay) {
        overlay.painting.set(self.frames.borrow().shown);
        overlay
            .drawing
            .update(self.surfaces.cx, |_, cx| cx.notify());
    }

    fn hide(&mut self, overlay: &Overlay) -> Result<(), OpenError> {
        #[cfg(test)]
        if let Some(result) = tests::recovery::native(tests::recovery::Operation::Hide) {
            return result.map_err(OpenError::from);
        }
        hide(overlay.window, self.surfaces.cx)
    }

    fn close(&mut self, overlay: Overlay) -> Result<(), OpenError> {
        overlay.painting.set(false);
        overlay
            .window
            .update(self.surfaces.cx, |_, window, _| window.remove_window())
            .map_err(|error| OpenError::Gpui(error.into()))
    }
}

/// Opens the hidden, transparent, click-through overlay, which never takes focus.
fn open(
    frames: Rc<RefCell<Frames>>,
    owner: WeakEntity<Presence>,
    scratch: Rc<RefCell<Scratch>>,
    notice: Rc<PaintNotice>,
    cx: &mut App,
    configure: impl FnOnce(&mut Window, &mut App) -> Result<(), platform::PresenceError>,
) -> Result<Overlay, OpenError> {
    let painting = Rc::new(Cell::new(false));
    let gate = Rc::clone(&painting);
    let (window, drawing) = gpui_kit::open_window(options(), cx, |_, cx| {
        cx.new(|_| Drawing {
            frames,
            owner,
            painting: gate,
            scratch,
            notice,
        })
    })
    .map_err(|error| OpenError::Gpui(error.into()))?;
    let configured = window
        .update(cx, |_, window, cx| configure(window, cx))
        .map_err(|error| OpenError::Gpui(error.into()))
        .and_then(|result| result.map_err(OpenError::from));
    if let Err(error) = configured {
        window
            .update(cx, |_, window, _| window.remove_window())
            .map_err(|error| OpenError::Gpui(error.into()))?;
        return Err(error);
    }
    Ok(Overlay {
        window,
        drawing,
        painting,
    })
}

/// Covers the primary display from its logical origin, topmost, without activating.
pub(super) fn show(window: AnyWindowHandle, cx: &mut App) -> Result<(), OpenError> {
    let display = cx
        .primary_display()
        .ok_or_else(|| OpenError::Gpui("no primary display".into()))?;
    let logical = display.bounds().size;
    window
        .update(cx, |_, window, cx| {
            let scale = window.scale_factor();
            let [width, height] =
                [logical.width, logical.height].map(|side| scaled(side.into(), scale));
            platform::place_presence_window(window, [0, 0, width, height])?;
            window.bounds_changed(cx);
            Ok::<_, platform::PresenceError>(())
        })
        .map_err(|error| OpenError::Gpui(error.into()))??;
    Ok(())
}

pub(super) fn hide(window: AnyWindowHandle, cx: &mut App) -> Result<(), OpenError> {
    window
        .update(cx, |_, window, _| platform::hide_presence_window(window))
        .map_err(|error| OpenError::Gpui(error.into()))??;
    Ok(())
}

/// Paints one item's primitives in window logical pixels, which are primary-display logical
/// pixels, with its transform, opacity, color, reveal and dash phase. `prims` starts at
/// primitive `first` of the item, and its fills come from `scene`, staged from its view.
pub(super) fn paint_item(
    item: &FrameItem,
    first: usize,
    prims: &[Prim],
    scene: &mut PaintScene,
    scratch: &mut Scratch,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), BuildError> {
    assemble(item, first, prims, scene, scratch)?;
    let PaintScene {
        pending, labels, ..
    } = scene;
    for piece in pending.drain(..) {
        match piece {
            Pending::Quad(quad) => {
                let _gpui = paint_phase::enter(Phase::Gpui);
                window.paint_quad(quad);
            }
            Pending::Path(path, paint) => {
                let ink = color(paint, item.color, item.opacity);
                let _gpui = paint_phase::enter(Phase::Gpui);
                window.paint_path(path, ink);
            }
            Pending::Text(index) => {
                let entry = labels.get_mut(index).ok_or(BuildError::Stale)?;
                let local = entry.prim.checked_sub(first).ok_or(BuildError::Stale)?;
                let prim = prims.get(local).ok_or(BuildError::Stale)?;
                paint_text(item, prim, entry, &mut scratch.fonts, window, cx)?;
            }
        }
    }
    Ok(())
}

fn assemble(
    item: &FrameItem,
    first: usize,
    prims: &[Prim],
    scene: &mut PaintScene,
    scratch: &mut Scratch,
) -> Result<(), BuildError> {
    scene.pending.clear();
    if !item.opacity.is_finite() || item.opacity <= 0.0 || !item.draw.is_finite() {
        return Ok(());
    }
    let result = (|| {
        for (local, prim) in prims.iter().enumerate() {
            let quad = match prim {
                Prim::Rect {
                    rect,
                    radius,
                    fill,
                    border,
                } => upright(
                    item.transform,
                    item.draw,
                    *rect,
                    *radius,
                    fill.map(|ink| color(ink, item.color, item.opacity)),
                    border.map(|(ink, width)| (color(ink, item.color, item.opacity), width)),
                ),
                _ => None,
            };
            let pieces = if let Some(quad) = quad {
                [Some(Pending::Quad(quad)), None]
            } else {
                match prim {
                    Prim::Text { .. } => [
                        Some(Pending::Text(
                            scene.label_index(item.handle, first + local)?,
                        )),
                        None,
                    ],
                    Prim::Fill { color, .. } => [
                        scene
                            .fill(item.handle, first + local, item.transform)?
                            .map(|path| Pending::Path(path, *color)),
                        None,
                    ],
                    _ => {
                        paint_geometry::paths(scratch, prim, item.transform, item.draw, item.flow)?
                            .map(|each| each.map(|(path, paint)| Pending::Path(path, paint)))
                    }
                }
            };
            for piece in pieces.into_iter().flatten() {
                if scene.pending.len() == scene.pending.capacity() {
                    return Err(BuildError::Stale);
                }
                scene.pending.push(piece);
            }
        }
        Ok(())
    })();
    if result.is_err() {
        scene.pending.clear();
    }
    result
}

/// A rect's native quad while its transform keeps it axis-aligned and upright and it is fully
/// drawn, with radius and border clamped as its rounded-outline paths clamp them.
fn upright(
    transform: Affine,
    draw: f32,
    rect: Rect,
    radius: f32,
    fill: Option<Hsla>,
    border: Option<(Hsla, f32)>,
) -> Option<PaintQuad> {
    let [a, b, c, d, ..] = transform.0;
    if b.abs() >= f32::EPSILON || c.abs() >= f32::EPSILON || a <= 0.0 || d <= 0.0 || draw < 1.0 {
        return None;
    }
    let half = (rect.size.x.min(rect.size.y) * 0.5).max(0.0);
    let zoom = a.min(d);
    let (ink, width) = border.map_or((transparent_black(), 0.0), |(ink, width)| {
        (ink, width.clamp(0.0, half))
    });
    let world = transform.bounds(rect);
    Some(quad(
        Bounds::new(
            point(px(world.origin.x), px(world.origin.y)),
            size(px(world.size.x), px(world.size.y)),
        ),
        px(radius.clamp(0.0, half) * zoom),
        fill.unwrap_or_else(transparent_black),
        px(width * zoom),
        ink,
        BorderStyle::Solid,
    ))
}

/// A prim's paint with the item's color override, which keeps the prim's own opacity, and the
/// item's opacity multiplied in.
fn color(paint: Paint, over: Option<Paint>, opacity: f32) -> Hsla {
    let paint = over.map_or(paint, |over| over.faded(paint.opacity));
    Rgba {
        a: (paint.opacity * opacity).clamp(0.0, 1.0),
        ..rgb(paint.rgb.get())
    }
    .into()
}

/// Where a label paints and at what font and wrap size: at its transformed anchor, scaled by
/// the transform's uniform scale.
fn label(
    transform: Affine,
    origin: Vec2,
    font_size: f32,
    max_width: Option<f32>,
) -> (Point<Pixels>, Pixels, Option<Pixels>) {
    // shortcut: labels stay upright because pinned GPUI paint_glyph has no transform; upgrade through a glyph transform (OSC-217)
    let zoom = paint_paths::scale(transform);
    let at = transform.point(origin);
    (
        point(px(at.x), px(at.y)),
        px(font_size * zoom),
        max_width.map(|width| px(width * zoom)),
    )
}

/// Shapes a label with the font and line height preparation measured it with, then paints it.
#[expect(clippy::similar_names, reason = "the shaper and the label it shaped")]
fn paint_text(
    item: &FrameItem,
    prim: &Prim,
    entry: &mut paint_cache::LabelEntry,
    fonts: &mut paint_cache::Fonts,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), BuildError> {
    let Prim::Text {
        origin,
        style,
        color: ink,
        max_width,
        align,
        ..
    } = prim
    else {
        unreachable!("only text prims become text pieces");
    };
    let family = {
        let _gpui = paint_phase::enter(Phase::Gpui);
        cx.theme().font_family.clone()
    };
    let (mut origin, font_size, wrap) = label(item.transform, *origin, style.size, *max_width);
    let shaper = {
        let text_system = {
            let _gpui = paint_phase::enter(Phase::Gpui);
            window.text_system()
        };
        Arc::clone(text_system)
    };
    let shaped = {
        let _shape = paint_phase::enter(Phase::GpuiShape);
        entry.shape(
            fonts,
            &family,
            &shaper,
            font_size,
            wrap,
            window.scale_factor(),
        )?
    };
    let align = match align {
        Align::Left => TextAlign::Left,
        Align::Center => TextAlign::Center,
        Align::Right => TextAlign::Right,
    };
    for line in &shaped.lines {
        let run = [DecorationRun {
            len: u32::try_from(line.len()).map_err(|_| BuildError::Label)?,
            color: color(*ink, item.color, item.opacity),
            background_color: None,
            underline: None,
            strikethrough: None,
        }];
        {
            let _gpui = paint_phase::enter(Phase::Gpui);
            if line.wrap_boundaries.is_empty() {
                line.unwrapped_layout
                    .paint(origin, shaped.line_height, align, wrap, &run, window, cx)
                    .map_err(|_| BuildError::Label)?;
            } else {
                paint_rows(
                    line,
                    origin,
                    shaped.line_height,
                    align,
                    wrap,
                    run[0].color,
                    window,
                    cx,
                )?;
            }
        }
        let height = {
            let _gpui = paint_phase::enter(Phase::Gpui);
            line.size(shaped.line_height).height
        };
        origin.y += height;
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "matches the pinned GPUI line paint inputs"
)]
fn paint_rows(
    line: &WrappedLine,
    origin: Point<Pixels>,
    line_height: Pixels,
    align: TextAlign,
    width: Option<Pixels>,
    ink: Hsla,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), BuildError> {
    let layout = &line.unwrapped_layout;
    let width = width.or(line.wrap_width).unwrap_or(layout.width);
    let mut bounds = Bounds::new(
        origin,
        size(layout.width, line_height * (line.wrap_boundaries.len() + 1)),
    );
    if align != TextAlign::Left && layout.len != 0 {
        let mut start = px(0.0);
        for (row, end) in line
            .wrap_boundaries
            .iter()
            .map(|boundary| {
                layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix]
                    .position
                    .x
            })
            .chain([layout.width])
            .enumerate()
        {
            let row_width = end - start;
            let offset = match align {
                TextAlign::Left => px(0.0),
                TextAlign::Center => (width - row_width) / 2.0,
                TextAlign::Right => width - row_width,
            };
            bounds = bounds.union(&Bounds::new(
                point(origin.x + offset, origin.y + line_height * row),
                size(row_width, line_height),
            ));
            start = end;
        }
    }
    window.paint_layer(bounds, |window| {
        let baseline = point(
            px(0.0),
            (line_height - layout.ascent - layout.descent) / 2.0 + layout.ascent,
        );
        let mut wraps = line.wrap_boundaries.iter().peekable();
        let mut at = point(
            row_origin(line, origin.x, width, px(0.0), align, wraps.peek().copied()),
            origin.y,
        );
        let mut previous = point(px(0.0), px(0.0));
        for (run_ix, run) in layout.runs.iter().enumerate() {
            let glyph_size = cx
                .text_system()
                .bounding_box(run.font_id, layout.font_size)
                .size;
            for (glyph_ix, glyph) in run.glyphs.iter().enumerate() {
                at.x += glyph.position.x - previous.x;
                if wraps.peek() == Some(&&WrapBoundary { run_ix, glyph_ix }) {
                    wraps.next();
                    at.x = row_origin(
                        line,
                        origin.x,
                        width,
                        glyph.position.x,
                        align,
                        wraps.peek().copied(),
                    );
                    at.y += line_height;
                }
                previous = glyph.position;
                if Bounds::new(at, glyph_size).intersects(&window.content_mask().bounds) {
                    let at = at + baseline + point(px(0.0), glyph.position.y);
                    let result = if glyph.is_emoji {
                        window.paint_emoji(at, run.font_id, glyph.id, layout.font_size)
                    } else {
                        window.paint_glyph(at, run.font_id, glyph.id, layout.font_size, ink)
                    };
                    result.map_err(|_| BuildError::Label)?;
                }
            }
        }
        Ok(())
    })
}

fn row_origin(
    line: &WrappedLine,
    origin: Pixels,
    width: Pixels,
    start: Pixels,
    align: TextAlign,
    boundary: Option<&WrapBoundary>,
) -> Pixels {
    let layout = &line.unwrapped_layout;
    let end = boundary.map_or(layout.width, |boundary| {
        layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix]
            .position
            .x
    });
    match align {
        TextAlign::Left => origin,
        TextAlign::Center => (origin * 2.0 + width - (end - start)) / 2.0,
        TextAlign::Right => origin + width - (end - start),
    }
}

#[cfg(test)]
#[path = "overlay_tests.rs"]
mod tests;
