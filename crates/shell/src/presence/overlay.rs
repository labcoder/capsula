//! The drawing overlay: one transparent, click-through window over the primary display that
//! paints the presenter's sampled frames, shown only while one has something to paint.
// shortcut: primary display only; add per-display windows once a second display can be certified

use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};

use async_channel::Sender;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, BorderStyle, Bounds, Context, Entity, Font, FontWeight,
    Hsla, IntoElement, PaintQuad, Path, Pixels, Point, Render, Rgba, Styled as _, TextAlign,
    TextRun, WeakEntity, Window, canvas, point, px, quad, rgb, size, transparent_black,
};
use kumochi_scene::{
    FrameItem,
    display::{Paint, Prim},
    geom::{Affine, Rect, Vec2},
    record::Align,
};

use super::paint_paths::{self, BuildError};
use super::{OpenError, Presence, configure_surface, options, scaled};
use crate::platform::{self, PresenceEvent, PresenceSurface};
use crate::presenter::{Frames, Sample};

/// The overlay window and its root, opened on the first visible drawing and then reused.
#[derive(Clone)]
pub(super) struct Overlay {
    pub(super) window: AnyWindowHandle,
    drawing: Entity<Drawing>,
}

/// The overlay's root: every sampled frame, in scene order.
struct Drawing {
    frames: Rc<RefCell<Frames>>,
    owner: WeakEntity<Presence>,
}

impl Render for Drawing {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        let frames = Rc::clone(&self.frames);
        let owner = self.owner.clone();
        canvas(
            |_, _, _| (),
            move |_, (), window, cx| {
                let failed = frames.borrow().paint(|frame| {
                    frame
                        .paint_items()
                        .try_for_each(|(item, prims)| paint_item(item, prims, window, cx))
                });
                if failed {
                    cx.defer(move |cx| {
                        let result = owner.update(cx, |this, cx| {
                            let effects = this.presenter.paint_failed();
                            this.scene_effects(Ok(effects), cx);
                            this.runs_changed(cx)
                        });
                        if let Ok(Err(error)) = result {
                            tracing::error!(%error, "can't follow a drawing that failed to paint");
                        }
                    });
                }
            },
        )
        .size_full()
    }
}

impl Presence {
    /// Follows a change in the mirrored scenes: samples them, then shows the overlay while a
    /// frame has something to paint and hides it otherwise. At rest it asks for no frames.
    pub(super) fn draw_changed(&mut self, cx: &mut Context<'_, Self>) {
        // B5's views rest, so every instant samples the same frame; B6 passes its motion clock.
        let (sample, effects) = self.presenter.sample(0);
        if !effects.is_empty() {
            self.scene_effects(Ok(effects), cx);
            return;
        }
        if let Err(error) = self.sync_overlay(sample, cx) {
            tracing::error!(%error, "the drawing overlay couldn't update");
            // A drawing that can't be shown is cleared rather than kept off screen.
            if sample.visible {
                let effects = self.presenter.clear(Instant::now());
                self.scene_effects(effects, cx);
            }
        }
    }

    fn sync_overlay(
        &mut self,
        sample: Sample,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        let frames = self.presenter.frames();
        let overlay = match self.overlay.clone() {
            Some(overlay) => overlay,
            None if sample.visible => {
                let opened = open(
                    Rc::clone(&frames),
                    cx.weak_entity(),
                    self.events.clone(),
                    cx,
                )?;
                self.overlay.insert(opened).clone()
            }
            None => return Ok(()),
        };
        let shown = frames.borrow().shown;
        if sample.visible && !shown {
            show(overlay.window, cx)?;
            frames.borrow_mut().shown = true;
        } else if !sample.visible && shown {
            frames.borrow_mut().shown = false;
            // Repaint the empty frames first, so a failed hide leaves nothing on screen.
            overlay.drawing.update(cx, |_, cx| cx.notify());
            return hide(overlay.window, cx);
        } else if !sample.changed {
            return Ok(());
        }
        overlay.drawing.update(cx, |_, cx| cx.notify());
        Ok(())
    }
}

/// Opens the hidden, transparent, click-through overlay, which never takes focus.
fn open(
    frames: Rc<RefCell<Frames>>,
    owner: WeakEntity<Presence>,
    events: Sender<PresenceEvent>,
    cx: &mut App,
) -> Result<Overlay, OpenError> {
    let (window, drawing) =
        gpui_kit::open_window(options(), cx, |_, cx| cx.new(|_| Drawing { frames, owner }))
            .map_err(|error| OpenError::Gpui(error.into()))?;
    window
        .update(cx, |_, window, cx| {
            configure_surface(window, PresenceSurface::Overlay, events, cx)
        })
        .map_err(|error| OpenError::Gpui(error.into()))??;
    Ok(Overlay { window, drawing })
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

/// One primitive, built before any of its item paints.
enum Piece {
    Quad(PaintQuad),
    Paths(Vec<(Path<Pixels>, Paint)>),
    Text,
}

/// Paints one item's primitives in window logical pixels, which are primary-display logical
/// pixels, with its transform, opacity, color, reveal and dash phase.
pub(super) fn paint_item(
    item: &FrameItem,
    prims: &[Prim],
    window: &mut Window,
    cx: &mut App,
) -> Result<(), BuildError> {
    let pieces = pieces(item, prims)?;
    for (prim, piece) in prims.iter().zip(pieces) {
        match piece {
            Piece::Quad(quad) => window.paint_quad(quad),
            Piece::Paths(paths) => {
                for (path, paint) in paths {
                    window.paint_path(path, color(paint, item.color, item.opacity));
                }
            }
            Piece::Text => paint_text(item, prim, window, cx)?,
        }
    }
    Ok(())
}

/// Every primitive of an item, built in full before any of it paints, so a primitive that fails
/// paints none of the item. Returns an empty Vec while it can't show: opacity not finite and
/// positive, or a non-finite draw, which would otherwise take the fully drawn branches.
fn pieces(item: &FrameItem, prims: &[Prim]) -> Result<Vec<Piece>, BuildError> {
    if !item.opacity.is_finite() || item.opacity <= 0.0 || !item.draw.is_finite() {
        return Ok(Vec::new());
    }
    let ink = |paint| color(paint, item.color, item.opacity);
    let mut pieces = Vec::with_capacity(prims.len());
    for prim in prims {
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
                fill.map(ink),
                border.map(|(paint, width)| (ink(paint), width)),
            ),
            _ => None,
        };
        pieces.push(match (quad, prim) {
            (Some(quad), _) => Piece::Quad(quad),
            (None, Prim::Text { .. }) => Piece::Text,
            (None, _) => Piece::Paths(paint_paths::paths(
                prim,
                item.transform,
                item.draw,
                item.flow,
            )?),
        });
    }
    Ok(pieces)
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
fn paint_text(
    item: &FrameItem,
    prim: &Prim,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), BuildError> {
    let Prim::Text {
        origin,
        text,
        style,
        color: ink,
        max_width,
        align,
    } = prim
    else {
        unreachable!("only text prims become text pieces");
    };
    let font = Font {
        family: cx.theme().font_family.clone(),
        weight: if style.bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        ..Font::default()
    };
    let (mut origin, font_size, wrap) = label(item.transform, *origin, style.size, *max_width);
    let shaper = Arc::clone(window.text_system());
    let font_id = shaper.resolve_font(&font);
    let line_height = px(f32::from(shaper.ascent(font_id, font_size)).abs()
        + f32::from(shaper.descent(font_id, font_size)).abs());
    let runs = [TextRun {
        len: text.as_str().len(),
        font,
        color: color(*ink, item.color, item.opacity),
        ..TextRun::default()
    }];
    // shortcut: copies each label per paint because Text owns a Box<str>; share it if B6 motion measures this hot
    let lines = shaper
        .shape_text(
            text.as_str().to_owned().into(),
            font_size,
            &runs,
            wrap,
            None,
        )
        .map_err(|_| BuildError::Label)?;
    let align = match align {
        Align::Left => TextAlign::Left,
        Align::Center => TextAlign::Center,
        Align::Right => TextAlign::Right,
    };
    for line in &lines {
        let area = wrap.map(|width| Bounds::new(origin, size(width, line_height)));
        line.paint(origin, line_height, align, area, window, cx)
            .map_err(|_| BuildError::Label)?;
        origin.y += line.size(line_height).height;
    }
    Ok(())
}

#[cfg(test)]
#[path = "overlay_tests.rs"]
mod tests;
