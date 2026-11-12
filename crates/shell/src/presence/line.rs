//! The dock at rest: a short line near the work area's edge. GPUI draws it as an antialiased
//! rounded rect, and the hit proxy owns it plus a trigger band above and below, so the pointer
//! finds it without the line growing. A placement becomes current only once Windows accepts its
//! mask, so the line never shows where the mask isn't.

use std::sync::Arc;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Context, IntoElement, ObjectFit, ParentElement as _, Render, RenderImage, SharedString,
    Styled as _, StyledImage as _, WeakEntity, Window, canvas, div, img, px,
};

use super::diagnostics::{self, PendingPaint};
use super::geometry::Layout;
use super::kumo_view::PullSite;
use super::observer::{self, Drawn, Surface};
use super::paint::Stamp;
use super::pull_picture::PullPicture;
use super::{OpenError, Presence, platform};
use crate::platform::{PresenceError, PresenceNative, ROW_SPANS, RowSpans};

/// The line's size, in logical pixels.
const LINE: [f32; 2] = [56.0, 3.0];
/// The widest the drop label gets, in logical pixels; a longer title is cut short.
const LABEL_WIDTH: f32 = 320.0;
/// How far above and below the line the pointer still finds it, in logical pixels.
const TRIGGER: f32 = 8.0;
/// Failed attempts in a row before a placement stops retrying; a new position choice starts a
/// fresh run.
const MAX_FAILED_UPLOADS: u8 = 3;

/// The dock's slot, in physical pixels: the line and its trigger band with a pixel to spare on
/// every side for rounding.
pub(super) fn slot(scale: f32) -> [i32; 2] {
    [LINE[0], LINE[1] + 2.0 * TRIGGER].map(|value| whole(f64::from(value * scale).ceil()) + 2)
}

/// One placement of the line. Both halves come from the same slot and publish together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LineFrame {
    /// The line's device pixels, `[x, y, width, height]`: each edge rounded half toward zero,
    /// as GPUI snaps the bounds it draws. Only these pixels are drawn.
    pub(super) paint: [i32; 4],
    /// `paint` with its snapped edges grown by the trigger band above and below, rounded outward. Only these pixels
    /// are owned; every row holds the same span.
    pub(super) hit: [i32; 4],
}

impl LineFrame {
    /// The line with its top-left at `origin`, in physical screen pixels, at `scale`.
    pub(super) fn at([x, y]: [f64; 2], scale: f32) -> Self {
        let [width, height] = LINE.map(|value| f64::from(value * scale));
        let trigger = f64::from(TRIGGER * scale);
        let snap = |value: f64| whole((value - 0.5).ceil());
        let [left, top, right, bottom] = [x, y, x + width, y + height].map(snap);
        let hit_top = whole((f64::from(top) - trigger).floor());
        let hit_bottom = whole((f64::from(bottom) + trigger).ceil());
        Self {
            paint: [left, top, right - left, bottom - top],
            hit: [left, hit_top, right - left, hit_bottom - hit_top],
        }
    }

    /// The line centred in the dock's `slot`, `[x, y, width, height]`.
    pub(super) fn in_slot([x, y, width, height]: [i32; 4], scale: f32) -> Self {
        let [line_width, line_height] = LINE.map(|value| f64::from(value * scale));
        Self::at(
            [
                f64::from(x) + (f64::from(width) - line_width) / 2.0,
                f64::from(y) + (f64::from(height) - line_height) / 2.0,
            ],
            scale,
        )
    }

    /// Row `y`'s owned pixels, as the hit mask takes them.
    pub(super) fn row_spans(&self, y: i32) -> RowSpans {
        let [left, top, width, height] = self.hit;
        let mut spans = [None; ROW_SPANS];
        spans[0] = (top..top + height)
            .contains(&y)
            .then_some([left, left + width]);
        spans
    }

    pub(super) fn contains(&self, [x, y]: [i32; 2]) -> bool {
        let [left, top, width, height] = self.hit;
        (left..left + width).contains(&x) && (top..top + height).contains(&y)
    }
}

/// One placement of the dock: its layout, the line in its slot and the tray's position entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Placement {
    pub(super) layout: Layout,
    pub(super) frame: LineFrame,
    pub(super) index: usize,
}

/// The native surfaces a placement must bring into line: the hit proxy's mask, the overlay
/// window and the proxy's visibility. Each call either does its whole change or none of it.
pub(super) trait Surfaces {
    type Error: std::fmt::Display;
    /// Gives the proxy `frame`'s band.
    fn upload(&mut self, frame: &LineFrame) -> Result<(), Self::Error>;
    /// Moves the overlay to `overlay` and shows it.
    fn move_overlay(&mut self, overlay: [i32; 4]) -> Result<(), Self::Error>;
    /// Shows the proxy and checks `index` in the tray.
    fn show(&mut self, index: usize) -> Result<(), Self::Error>;
    /// Hides the overlay.
    fn hide_overlay(&mut self) -> Result<(), Self::Error>;
    /// Hides the proxy, so it takes no pointer input.
    fn hide_proxy(&mut self) -> Result<(), Self::Error>;
    /// Empties the proxy's mask over `hit`, so it takes no pointer input even while shown.
    fn clear_mask(&mut self, hit: [i32; 4]) -> Result<(), Self::Error>;
}

/// What Windows holds now, as far as the surfaces' last successful calls say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Held {
    /// The line's band, when the mask Windows holds is exactly it.
    mask: Option<LineFrame>,
    /// Where the mask Windows holds takes clicks, whoever gave it: the line's band or a
    /// pull's area. Failing closed empties it.
    proxy: Option<[i32; 4]>,
    /// None while the overlay is hidden.
    overlay: Option<[i32; 4]>,
    /// The tray entry, None while the proxy is hidden.
    shown: Option<usize>,
}

/// What one attempt at the requested placement did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Attempt<E> {
    /// Nothing is requested, or its attempts ran out.
    Idle,
    /// The mask, the overlay and the proxy all hold it; it's current now.
    Placed(Placement),
    /// It failed. `retry` says whether an attempt is left; `closed` that the attempts ran out
    /// with the surfaces apart, so the dock was hidden.
    Failed {
        error: E,
        first: bool,
        retry: bool,
        closed: bool,
    },
    /// The attempts ran out and the proxy could be neither hidden nor emptied, so it may still
    /// take clicks over a line nobody sees.
    Stranded(E),
}

/// The dock's placement as Windows accepted it, the one asked for since, and what Windows
/// holds in between.
#[derive(Debug, Default)]
pub(super) struct Line {
    /// Held by the mask, the overlay and the proxy, and drawn.
    accepted: Option<Placement>,
    /// Asked for and not yet accepted.
    requested: Option<Placement>,
    held: Held,
    /// Failed attempts at `requested` in a row.
    failed: u8,
    /// A pull is under way, from its accepted grab until it hands the dock back: it uses this
    /// line's coordinates and may hold the overlay and the mask, so the line takes no attempt.
    pulled: bool,
}

impl Line {
    pub(super) fn position_index(&self) -> Option<usize> {
        self.requested
            .or(self.accepted)
            .map(|placement| placement.index)
    }

    /// The line Windows last accepted.
    pub(super) fn accepted_frame(&self) -> Option<LineFrame> {
        self.accepted.map(|placement| placement.frame)
    }

    /// A grab was accepted: from now until the pull hands the dock back, it's drawn in this
    /// line's coordinates, so the line takes no attempt.
    pub(super) fn pull_started(&mut self) {
        self.pulled = true;
    }

    /// A pull moved the overlay: the next attempt moves it back to the line's.
    pub(super) fn pull_overlay(&mut self) {
        self.held.overlay = None;
        self.pulled = true;
    }

    /// Windows accepted a pull's mask over `area`: the next attempt gives the line its band
    /// back, and failing closed empties the pull's area.
    pub(super) fn pull_mask(&mut self, area: [i32; 4]) {
        self.held.mask = None;
        self.held.proxy = Some(area);
        self.pulled = true;
    }

    /// The pull is done with the mask and the overlay, and the dock draws no picture of it:
    /// ask for the line again, the pending placement if there is one, with a fresh run of
    /// attempts.
    pub(super) fn reclaim(&mut self) {
        self.pulled = false;
        self.requested = self.requested.or(self.accepted);
        self.failed = 0;
    }

    /// What the mask owns now: the pixels that take the pointer.
    pub(super) fn contains(&self, point: [i32; 2]) -> bool {
        self.accepted
            .is_some_and(|placement| placement.frame.contains(point))
    }

    /// A new choice replaces any pending one and gets a fresh run of attempts.
    pub(super) fn request(&mut self, placement: Placement) {
        self.requested = Some(placement);
        self.failed = 0;
    }

    /// Brings the surfaces to the requested placement, finishing from wherever an earlier
    /// attempt left them. It's current only once all three hold it. When the last attempt
    /// fails with the surfaces apart, the dock is hidden rather than left showing a line it
    /// doesn't own or owning a band it doesn't show.
    pub(super) fn attempt<S: Surfaces>(&mut self, surfaces: &mut S) -> Attempt<S::Error> {
        let Some(target) = self
            .requested
            .filter(|_| !self.pulled && self.failed < MAX_FAILED_UPLOADS)
        else {
            return Attempt::Idle;
        };
        if let Err(error) = self.converge(target, surfaces) {
            self.failed += 1;
            let first = self.failed == 1;
            let retry = self.failed < MAX_FAILED_UPLOADS;
            let closed = !retry && !self.coherent();
            if closed && let Err(stranded) = self.close(surfaces) {
                return Attempt::Stranded(stranded);
            }
            return Attempt::Failed {
                error,
                first,
                retry,
                closed,
            };
        }
        self.accepted = self.requested.take();
        self.failed = 0;
        Attempt::Placed(target)
    }

    fn converge<S: Surfaces>(
        &mut self,
        target: Placement,
        surfaces: &mut S,
    ) -> Result<(), S::Error> {
        if self.held.mask != Some(target.frame) {
            surfaces.upload(&target.frame)?;
            self.held.mask = Some(target.frame);
            self.held.proxy = Some(target.frame.hit);
        }
        let overlay = target.layout.overlay;
        if self.held.overlay != Some(overlay) {
            if let Err(error) = surfaces.move_overlay(overlay) {
                // Put the band back under the line still on screen; if that's refused too, the
                // next attempt finishes the move under the mask Windows holds.
                if let Some(accepted) = self.accepted
                    && self.held.mask != Some(accepted.frame)
                {
                    match surfaces.upload(&accepted.frame) {
                        Ok(()) => {
                            self.held.mask = Some(accepted.frame);
                            self.held.proxy = Some(accepted.frame.hit);
                        }
                        Err(restore) => tracing::warn!(%restore, "can't put the hit mask back"),
                    }
                }
                return Err(error);
            }
            self.held.overlay = Some(overlay);
        }
        if self.held.shown != Some(target.index) {
            surfaces.show(target.index)?;
            self.held.shown = Some(target.index);
        }
        Ok(())
    }

    /// Fails closed and starts fresh. The overlay hides if it can: one that can't shows nothing
    /// once its line is cleared. The proxy must stop taking clicks, hidden or emptied; when
    /// neither works, that's the error.
    fn close<S: Surfaces>(&mut self, surfaces: &mut S) -> Result<(), S::Error> {
        if let Err(error) = surfaces.hide_overlay() {
            tracing::warn!(%error, "can't hide the dock; its line is cleared instead");
        }
        if let Err(error) = surfaces.hide_proxy() {
            tracing::warn!(%error, "can't hide the dock's proxy; emptying its mask");
            if let Some(proxy) = self.held.proxy {
                surfaces.clear_mask(proxy)?;
            }
        }
        *self = Self::default();
        Ok(())
    }

    /// Whether what Windows holds is the accepted placement, or nothing shown at all.
    fn coherent(&self) -> bool {
        self.held
            == self.accepted.map_or(
                Held {
                    mask: self.held.mask,
                    proxy: self.held.proxy,
                    ..Held::default()
                },
                |accepted| Held {
                    mask: Some(accepted.frame),
                    proxy: Some(accepted.frame.hit),
                    overlay: Some(accepted.layout.overlay),
                    shown: Some(accepted.index),
                },
            )
    }
}

/// Runs `step` on `owner` at the window's next frame, with that frame's own window: GPUI
/// holds the window while its frame callbacks run, so a handle can't reach it then.
pub(super) fn on_next_frame<T: 'static>(
    window: &Window,
    owner: WeakEntity<T>,
    step: fn(&mut T, &mut Window, &mut Context<'_, T>),
) {
    window.on_next_frame(move |window, cx| {
        if owner.update(cx, |this, cx| step(this, window, cx)).is_err() {
            tracing::debug!("the presence closed before its next frame");
        }
    });
}

/// The real surfaces: the proxy's mask and visibility, and the dock's overlay window.
struct Desktop<'native, 'ui> {
    native: &'native mut PresenceNative,
    window: &'ui mut Window,
    cx: &'ui mut App,
    /// The observer's state after the last accepted upload, which the dock names when it draws.
    drawn: Drawn,
}

impl Surfaces for Desktop<'_, '_> {
    type Error = PresenceError;

    fn upload(&mut self, frame: &LineFrame) -> Result<(), PresenceError> {
        let begin_qpc = observer::begin(self.cx);
        let update = self.native.reshape(frame.hit, |y| frame.row_spans(y));
        let drawn = observer::commit(self.cx, begin_qpc, Some(frame), frame.hit, &update);
        update?;
        self.drawn = drawn;
        Ok(())
    }

    fn move_overlay(&mut self, overlay: [i32; 4]) -> Result<(), PresenceError> {
        platform::place_presence_window(self.window, overlay)?;
        self.window.bounds_changed(self.cx);
        Ok(())
    }

    fn show(&mut self, index: usize) -> Result<(), PresenceError> {
        self.native.show(index)
    }

    fn hide_overlay(&mut self) -> Result<(), PresenceError> {
        platform::hide_presence_window(self.window)
    }

    fn hide_proxy(&mut self) -> Result<(), PresenceError> {
        self.native.hide()
    }

    fn clear_mask(&mut self, hit: [i32; 4]) -> Result<(), PresenceError> {
        let begin_qpc = observer::begin(self.cx);
        let update = self.native.reshape(hit, |_| [None; ROW_SPANS]);
        let drawn = observer::commit(self.cx, begin_qpc, None, hit, &update);
        update?;
        self.drawn = drawn;
        Ok(())
    }
}

impl Presence {
    /// Asks for the dock at `placement` and tries it now; if it isn't current yet, the dock
    /// stays as it was and the next frames retry.
    ///
    /// # Errors
    /// Fails when the dock window is gone.
    pub(super) fn request_line(
        &mut self,
        placement: Placement,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), OpenError> {
        self.line.request(placement);
        let owner = cx.weak_entity();
        let handle = self.dock_window;
        handle
            .update(cx, |_, window, cx| self.try_line(owner, window, cx))
            .map_err(|error| OpenError::Gpui(error.into()))?;
        Ok(())
    }

    fn retry_line(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.try_line(cx.weak_entity(), window, cx)
            && let Err(error) = self.pointer_changed(cx)
        {
            tracing::debug!(%error, "can't recheck hover after the dock moved");
        }
    }

    /// One attempt at the requested placement. Returns whether it became current, with its
    /// layout and line published.
    pub(super) fn try_line(
        &mut self,
        owner: WeakEntity<Self>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let mut surfaces = Desktop {
            native: &mut self.native,
            window,
            cx,
            drawn: self.drawn,
        };
        let attempt = self.line.attempt(&mut surfaces);
        let Desktop {
            window, cx, drawn, ..
        } = surfaces;
        self.drawn = drawn;
        match attempt {
            Attempt::Idle => false,
            Attempt::Placed(placement) => {
                self.layout = placement.layout;
                if let Err(error) = self.reanchor_panels(cx) {
                    tracing::warn!(%error, "can't re-anchor open panels after the dock moved");
                }
                self.place_drawings(owner.clone(), cx);
                let origin = [placement.layout.overlay[0], placement.layout.overlay[1]];
                self.dock.update(cx, |dock, cx| {
                    dock.paint = Some(placement.frame.paint);
                    dock.origin = origin;
                    dock.drawn = drawn;
                    cx.notify();
                });
                let site = PullSite {
                    line: placement.frame.paint,
                    slot: placement.layout.dock,
                };
                self.kumo.update(cx, |kumo, _| kumo.set_site(Some(site)));
                self.line_reclaimed(cx);
                true
            }
            Attempt::Failed {
                error,
                first,
                retry,
                closed,
            } => {
                if first {
                    tracing::warn!(%error, "can't place the dock");
                }
                if closed {
                    tracing::error!(%error, "the dock is hidden until the next position choice");
                    self.dock.update(cx, |dock, cx| {
                        dock.paint = None;
                        cx.notify();
                    });
                    self.kumo.update(cx, |kumo, _| kumo.set_site(None));
                    self.line_reclaimed(cx);
                }
                if retry {
                    on_next_frame(window, owner, Self::retry_line);
                }
                false
            }
            Attempt::Stranded(error) => {
                tracing::error!(%error, "the dock's band may still take clicks, so the shell quits");
                cx.defer(move |cx| {
                    if owner.update(cx, Self::quit).is_err() {
                        tracing::debug!("the presence closed before it could quit");
                    }
                });
                false
            }
        }
    }
}

/// The dock window's view: the accepted line, drawn where Windows accepted its mask.
pub(super) struct Dock {
    /// Used only with `--input-diagnostics`; the next render takes it into a paint-phase marker.
    pub(super) pending_paint: PendingPaint,
    /// The accepted line's device pixels, or none before the first accepted upload.
    pub(super) paint: Option<[i32; 4]>,
    /// The dock window's top-left on screen, in physical pixels.
    pub(super) origin: [i32; 2],
    /// The accepted mask state `paint` came from, for `--gate-observer`.
    pub(super) drawn: Drawn,
    /// While Listening, the line takes the accent colour.
    pub(super) listening: bool,
    /// The pulled picture whose mask Windows accepted, drawn over the line.
    pub(super) pull: Option<PullPicture>,
    /// Images no picture draws any more, and those retired a frame earlier, which leave
    /// GPUI's atlas at this render.
    pub(super) retired: Vec<Arc<RenderImage>>,
    pub(super) aged: Vec<Arc<RenderImage>>,
    /// `pull` changed, to an accepted picture or to none as a pull lets go, and hasn't been
    /// rendered since.
    pub(super) undrawn: bool,
    /// The drop label under a carried body: screen text, drawn and never logged.
    pub(super) label: Option<SharedString>,
}

impl Dock {
    pub(super) fn new() -> Self {
        Self {
            pending_paint: PendingPaint::default(),
            paint: None,
            origin: [0; 2],
            drawn: Drawn::default(),
            listening: false,
            pull: None,
            // Clearing a pull's picture retires all it draws: drip, stub and body.
            retired: Vec::with_capacity(3),
            aged: Vec::with_capacity(3),
            undrawn: false,
            label: None,
        }
    }
}

impl Render for Dock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Recorded in GPUI's paint phase, not at present: PresentMon must join it to the screen.
        let marker = self.pending_paint.take().map(|at| {
            canvas(
                |_, _, _| (),
                move |_, (), _, cx| {
                    let painted_us = at.elapsed().as_micros();
                    diagnostics::record(cx, format_args!("listening painted_us={painted_us}"));
                },
            )
            .absolute()
            .size_full()
        });
        for image in self.aged.drain(..) {
            if let Err(error) = window.drop_image(image) {
                tracing::debug!(%error, "can't drop a pulled frame");
            }
        }
        std::mem::swap(&mut self.aged, &mut self.retired);
        self.undrawn = false;
        let scale = window.scale_factor();
        let fill = if self.listening {
            cx.theme().primary
        } else {
            cx.theme().background
        };
        let [origin_x, origin_y] = self.origin;
        let line = self.paint.map(|[x, y, width, height]| {
            div()
                .absolute()
                .left(logical(x - origin_x, scale))
                .top(logical(y - origin_y, scale))
                .w(logical(width, scale))
                .h(logical(height, scale))
                .rounded_full()
                .bg(fill)
                .border_1()
                .border_color(cx.theme().border)
        });
        let pulled = self
            .pull
            .iter()
            .flat_map(|picture| picture.images.iter().flatten());
        let pulled = pulled.map(|(quad, image)| {
            img(Arc::clone(image))
                .absolute()
                .left(logical(quad.x - origin_x, scale))
                .top(logical(quad.y - origin_y, scale))
                .w(logical(quad.width, scale))
                .h(logical(quad.height, scale))
                .object_fit(ObjectFit::Fill)
        });
        let body = self
            .pull
            .as_ref()
            .and_then(|picture| picture.images[2].as_ref());
        let label = self.label.clone().zip(body).map(|(text, (quad, _))| {
            div()
                .absolute()
                .left(logical(quad.x + quad.width / 2 - origin_x, scale) - px(LABEL_WIDTH / 2.0))
                .top(logical(quad.y + quad.height - origin_y, scale) + px(4.0))
                .w(px(LABEL_WIDTH))
                .flex()
                .justify_center()
                .child(
                    div()
                        .px_2()
                        .rounded_md()
                        .bg(cx.theme().popover.opacity(0.85))
                        .text_color(cx.theme().popover_foreground)
                        .text_sm()
                        .truncate()
                        .child(text),
                )
        });
        div()
            .size_full()
            .relative()
            .child(Stamp(Surface::Dock, self.drawn))
            .children(line)
            .children(pulled)
            .children(label)
            // After the frame's stamp, so the paint qpc it logs falls inside that frame.
            .children(marker)
    }
}

/// Physical pixels as GPUI's logical pixels.
#[expect(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    reason = "screen coordinates are far below f32's exact integer range"
)]
fn logical(physical: i32, scale: f32) -> gpui_kit::Pixels {
    px(physical as f32 / scale)
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "whole screen coordinates fit i32"
)]
fn whole(value: f64) -> i32 {
    value as i32
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_kit::{AppContext as _, Context, IntoElement, Render, Window, WindowOptions, div};

    use super::{Attempt, Line, LineFrame, Placement, Surfaces, on_next_frame, slot};
    use crate::platform::ROW_SPANS;
    use crate::presence::geometry::Layout;
    use crate::presence::pull_dock::{Holding, PullDock, Rest};

    /// The first and last owned pixel on each side, and the pass-through pixel beyond each.
    fn assert_edges(frame: &LineFrame, [left, top, right, bottom]: [i32; 4]) {
        let mid = [left.midpoint(right), top.midpoint(bottom)];
        for (owned, beyond) in [
            ([left, mid[1]], [left - 1, mid[1]]),
            ([right, mid[1]], [right + 1, mid[1]]),
            ([mid[0], top], [mid[0], top - 1]),
            ([mid[0], bottom], [mid[0], bottom + 1]),
        ] {
            assert!(frame.contains(owned), "{owned:?} is owned: {frame:?}");
            assert!(
                !frame.contains(beyond),
                "{beyond:?} passes through: {frame:?}"
            );
        }
    }

    #[test]
    fn at_125_percent_from_a_fractional_origin_the_band_owns_exactly_its_pixels() {
        // The line spans x 100.3..170.3 and y 20.6..24.35, so it draws columns 100..170 and rows
        // 21..24. The band is 8 × 1.25 = 10 rows above and below those rows: 11..34.
        let frame = LineFrame::at([100.3, 20.6], 1.25);
        assert_eq!(frame.paint, [100, 21, 70, 3]);
        assert_eq!(frame.hit, [100, 11, 70, 23]);
        assert_edges(&frame, [100, 11, 169, 33]);
        assert_eq!(frame.row_spans(11), [Some([100, 170]), None, None, None]);
        assert_eq!(frame.row_spans(10), [None; ROW_SPANS]);
        assert_eq!(frame.row_spans(34), [None; ROW_SPANS]);
    }

    #[test]
    fn at_150_percent_from_a_fractional_origin_the_band_owns_exactly_its_pixels() {
        // 200.5 and 284.5 round half toward zero, as GPUI snaps the bounds it draws.
        let frame = LineFrame::at([200.5, 7.25], 1.5);
        assert_eq!(frame.paint, [200, 7, 84, 5]);
        assert_eq!(frame.hit, [200, -5, 84, 29]);
        assert_edges(&frame, [200, -5, 283, 23]);
    }

    #[test]
    fn at_115_percent_the_band_grows_from_the_drawn_rows_and_fills_its_slot() {
        // The slot is ceil(64.4) + 2 by ceil(21.85) + 2. Centred in it the line spans x 1.3..65.7
        // and y 10.275..13.725, so it draws columns 1..66 and rows 10..14; the band is 9.2 rows
        // above and below those, rounded out to rows 0..24.
        assert_eq!(slot(1.15), [67, 24]);
        let frame = LineFrame::in_slot([0, 0, 67, 24], 1.15);
        assert_eq!(frame.paint, [1, 10, 65, 4]);
        assert_eq!(frame.hit, [1, 0, 65, 24]);
        assert_edges(&frame, [1, 0, 65, 23]);
    }

    #[test]
    fn the_line_sits_centred_in_its_slot_with_its_band_inside() {
        assert_eq!(slot(1.0), [58, 21]);
        let frame = LineFrame::in_slot([0, 0, 58, 21], 1.0);
        assert_eq!(frame.paint, [1, 9, 56, 3]);
        assert_eq!(frame.hit, [1, 1, 56, 19]);
        for scale in [1.0, 1.15, 1.25, 1.5, 1.75, 2.0] {
            let [width, height] = slot(scale);
            let [x, y, hit_width, hit_height] =
                LineFrame::in_slot([5, 7, width, height], scale).hit;
            assert!(
                x >= 5 && y >= 7 && x + hit_width <= 5 + width && y + hit_height <= 7 + height,
                "{scale}"
            );
        }
    }

    /// The dock at x, with its overlay over its slot.
    fn placement(x: i32, index: usize) -> Placement {
        let slot = [x, 0, 58, 21];
        Placement {
            layout: Layout {
                dock: slot,
                panel: [x, 30, 300, 140],
                overlay: slot,
            },
            frame: LineFrame::in_slot(slot, 1.0),
            index,
        }
    }

    /// Windows as the surfaces see it: each call refuses when its script says so, and changes
    /// nothing then.
    #[derive(Default)]
    struct Fake {
        mask: Option<LineFrame>,
        /// A pull's mask over this area, which the line's surfaces didn't give.
        pulled: Option<[i32; 4]>,
        overlay: Option<[i32; 4]>,
        shown: Option<usize>,
        refuse_uploads: Vec<bool>,
        refuse_moves: Vec<bool>,
        refuse_shows: Vec<bool>,
        refuse_overlay_hides: bool,
        refuse_proxy_hides: bool,
        refuse_clears: bool,
    }

    fn refused(script: &mut Vec<bool>) -> bool {
        !script.is_empty() && script.remove(0)
    }

    impl Surfaces for Fake {
        type Error = &'static str;

        fn upload(&mut self, frame: &LineFrame) -> Result<(), &'static str> {
            if refused(&mut self.refuse_uploads) {
                return Err("upload refused");
            }
            self.mask = Some(*frame);
            self.pulled = None;
            Ok(())
        }

        fn move_overlay(&mut self, overlay: [i32; 4]) -> Result<(), &'static str> {
            if refused(&mut self.refuse_moves) {
                return Err("move refused");
            }
            self.overlay = Some(overlay);
            Ok(())
        }

        fn show(&mut self, index: usize) -> Result<(), &'static str> {
            if refused(&mut self.refuse_shows) {
                return Err("show refused");
            }
            self.shown = Some(index);
            Ok(())
        }

        fn hide_overlay(&mut self) -> Result<(), &'static str> {
            if self.refuse_overlay_hides {
                return Err("overlay hide refused");
            }
            self.overlay = None;
            Ok(())
        }

        fn hide_proxy(&mut self) -> Result<(), &'static str> {
            if self.refuse_proxy_hides {
                return Err("proxy hide refused");
            }
            self.shown = None;
            Ok(())
        }

        fn clear_mask(&mut self, hit: [i32; 4]) -> Result<(), &'static str> {
            if self.refuse_clears {
                return Err("clear refused");
            }
            if self.pulled == Some(hit) {
                self.pulled = None;
                return Ok(());
            }
            if self.mask.map(|mask| mask.hit) != Some(hit) {
                return Err("cleared a band it doesn't hold");
            }
            self.mask = None;
            Ok(())
        }
    }

    impl Fake {
        /// The line on screen at `placement`, with the band under it and the proxy showing it.
        fn holds(&self, placement: Placement) -> bool {
            self.mask == Some(placement.frame)
                && self.overlay == Some(placement.layout.overlay)
                && self.shown == Some(placement.index)
        }

        /// Nothing on screen and nothing taking clicks.
        fn hidden(&self) -> bool {
            self.overlay.is_none() && self.shown.is_none()
        }

        /// A shown proxy with a band: it takes clicks.
        fn takes_clicks(&self) -> bool {
            self.shown.is_some() && (self.mask.is_some() || self.pulled.is_some())
        }
    }

    /// A line at A on a fake desktop.
    fn at_a() -> (Line, Fake) {
        let (mut line, mut fake) = (Line::default(), Fake::default());
        line.request(placement(0, 0));
        assert_eq!(line.attempt(&mut fake), Attempt::Placed(placement(0, 0)));
        (line, fake)
    }

    fn failed([retry, closed]: [bool; 2]) -> impl Fn(&Attempt<&'static str>) -> bool {
        move |attempt| matches!(attempt, Attempt::Failed { retry: r, closed: c, .. } if *r == retry && *c == closed)
    }

    #[test]
    fn a_position_chosen_after_a_grab_before_any_pull_frame_waits_and_never_fails_closed() {
        let (a, b) = (placement(0, 0), placement(400, 2));
        let (mut line, mut fake) = at_a();
        line.pull_started();
        line.request(b);
        // Each attempt would move the band, have its overlay move refused, and fail to put the
        // band back: the third would leave the surfaces apart and close the dock.
        fake.refuse_uploads = vec![false, true, false, true, false, true];
        fake.refuse_moves = vec![true; 3];
        for _ in 0..3 {
            assert_eq!(
                line.attempt(&mut fake),
                Attempt::Idle,
                "the pull uses A's line"
            );
        }
        assert!(
            fake.holds(a),
            "nothing on screen changed and the dock stays shown"
        );
        assert_eq!(
            (fake.refuse_uploads.len(), fake.refuse_moves.len()),
            (6, 3),
            "no surface was touched"
        );
        (fake.refuse_uploads, fake.refuse_moves) = (Vec::new(), Vec::new());
        line.reclaim();
        assert_eq!(
            line.attempt(&mut fake),
            Attempt::Placed(b),
            "placed at the hand-back"
        );
        assert!(fake.holds(b));
    }

    #[test]
    fn a_pull_keeps_its_mask_until_the_dock_draws_no_body_then_a_refused_shrink_keeps_a_band() {
        let (a, b) = (placement(0, 0), placement(400, 2));
        let (mut line, mut fake) = at_a();
        let area = [-228, -228, 514, 477];
        line.pull_overlay();
        (fake.mask, fake.pulled) = (None, Some(area));
        line.pull_mask(area);
        line.request(b);
        assert_eq!(
            line.attempt(&mut fake),
            Attempt::Idle,
            "the dock draws the body"
        );
        assert_eq!(
            (fake.mask, fake.pulled),
            (None, Some(area)),
            "the body's pixels stay owned"
        );
        let mut dock = PullDock::new(477);
        assert_eq!(dock.rest(Holding::Picture), Rest::Release);
        assert_eq!(dock.rest(Holding::Clearing), Rest::Wait);
        assert_eq!(dock.rest(Holding::Nothing), Rest::Reclaim);
        line.reclaim();
        fake.refuse_moves = vec![true];
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert_eq!(
            fake.mask,
            Some(a.frame),
            "the band under the line the dock still draws"
        );
        assert_eq!(line.attempt(&mut fake), Attempt::Placed(b));
        assert!(fake.holds(b));
    }

    #[test]
    fn a_pull_mask_a_failed_reclaim_leaves_is_emptied_when_the_proxy_wont_hide() {
        let (mut line, mut fake) = at_a();
        let area = [-228, -228, 514, 477];
        (fake.mask, fake.pulled) = (None, Some(area));
        line.pull_overlay();
        line.pull_mask(area);
        fake.refuse_uploads = vec![true; 3];
        fake.refuse_proxy_hides = true;
        line.reclaim();
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert!(failed([false, true])(&line.attempt(&mut fake)));
        assert_eq!(fake.pulled, None, "the pull's area was emptied");
        assert!(!fake.takes_clicks(), "nothing on screen takes clicks");
    }

    #[test]
    fn a_refused_upload_leaves_the_dock_where_it_was_until_a_retry_moves_it() {
        let (a, b) = (placement(0, 0), placement(400, 2));
        let (mut line, mut fake) = at_a();
        fake.refuse_uploads = vec![true];
        line.request(b);
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert!(fake.holds(a), "Windows still holds A");
        assert!(line.contains([1, 1]) && !line.contains([401, 1]));
        assert_eq!(line.attempt(&mut fake), Attempt::Placed(b));
        assert!(fake.holds(b));
        assert!(line.contains([401, 1]) && !line.contains([1, 1]));
    }

    #[test]
    fn a_refused_overlay_move_puts_the_mask_back_under_the_line() {
        let (a, b) = (placement(0, 0), placement(400, 2));
        let (mut line, mut fake) = at_a();
        fake.refuse_moves = vec![true];
        line.request(b);
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert!(fake.holds(a));
        assert_eq!(line.attempt(&mut fake), Attempt::Placed(b));
        assert!(fake.holds(b));
    }

    #[test]
    fn a_refused_rollback_is_finished_under_the_mask_windows_holds() {
        let b = placement(400, 2);
        let (mut line, mut fake) = at_a();
        fake.refuse_moves = vec![true];
        fake.refuse_uploads = vec![false, true];
        line.request(b);
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert_eq!(
            (fake.mask, fake.overlay),
            (Some(b.frame), Some([0, 0, 58, 21])),
            "the surfaces are apart"
        );
        assert_eq!(line.attempt(&mut fake), Attempt::Placed(b));
        assert!(fake.holds(b));
    }

    #[test]
    fn a_refused_show_is_retried_and_the_tray_follows() {
        let b = placement(400, 2);
        let (mut line, mut fake) = at_a();
        fake.refuse_shows = vec![true];
        line.request(b);
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert_eq!(fake.shown, Some(0));
        assert!(
            !line.contains([401, 1]),
            "B isn't current while its proxy isn't shown"
        );
        assert_eq!(line.attempt(&mut fake), Attempt::Placed(b));
        assert!(fake.holds(b));
    }

    #[test]
    fn a_proxy_that_never_shows_hides_the_dock() {
        let (mut line, mut fake) = (Line::default(), Fake::default());
        fake.refuse_shows = vec![true; 3];
        line.request(placement(0, 0));
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert!(failed([false, true])(&line.attempt(&mut fake)));
        assert!(fake.hidden(), "no line shows without its band");
        assert!(!line.contains([1, 1]));
        assert_eq!(line.attempt(&mut fake), Attempt::Idle);
    }

    #[test]
    fn a_refused_rollback_at_the_limit_hides_the_dock_until_a_new_choice() {
        let (b, c) = (placement(400, 2), placement(800, 5));
        let (mut line, mut fake) = at_a();
        fake.refuse_uploads = vec![true, true, false, true];
        fake.refuse_moves = vec![true];
        line.request(b);
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert!(failed([false, true])(&line.attempt(&mut fake)));
        assert!(fake.hidden(), "no band at B under a line at A");
        assert!(!line.contains([1, 1]) && !line.contains([401, 1]));
        assert_eq!(line.attempt(&mut fake), Attempt::Idle);
        line.request(c);
        assert_eq!(line.attempt(&mut fake), Attempt::Placed(c));
        assert!(fake.holds(c));
        assert!(line.contains([801, 1]));
    }

    /// The dock at A asked to move to B, with a proxy that won't show there: the last
    /// attempt fails closed.
    fn run_out(line: &mut Line, fake: &mut Fake) -> Attempt<&'static str> {
        fake.refuse_shows = vec![true; 3];
        line.request(placement(400, 2));
        assert!(failed([true, false])(&line.attempt(fake)));
        assert!(failed([true, false])(&line.attempt(fake)));
        line.attempt(fake)
    }

    #[test]
    fn an_overlay_that_wont_hide_still_closes_the_band() {
        let (mut line, mut fake) = at_a();
        fake.refuse_overlay_hides = true;
        assert!(failed([false, true])(&run_out(&mut line, &mut fake)));
        assert!(!fake.takes_clicks(), "the proxy is hidden");
        assert_eq!(fake.overlay, Some(placement(400, 2).layout.overlay));
        assert!(!line.contains([401, 1]) && !line.contains([1, 1]));
    }

    #[test]
    fn a_proxy_that_wont_hide_is_emptied_and_the_next_choice_places_it() {
        let c = placement(800, 5);
        let (mut line, mut fake) = at_a();
        fake.refuse_proxy_hides = true;
        assert!(failed([false, true])(&run_out(&mut line, &mut fake)));
        assert_eq!(
            (fake.shown, fake.mask),
            (Some(0), None),
            "shown with no band"
        );
        assert!(!fake.takes_clicks());
        fake.refuse_proxy_hides = false;
        line.request(c);
        assert_eq!(line.attempt(&mut fake), Attempt::Placed(c));
        assert!(fake.holds(c) && line.contains([801, 1]));
    }

    #[test]
    fn neither_surface_hiding_still_leaves_no_band_taking_clicks() {
        let (mut line, mut fake) = at_a();
        (fake.refuse_overlay_hides, fake.refuse_proxy_hides) = (true, true);
        assert!(failed([false, true])(&run_out(&mut line, &mut fake)));
        assert!(!fake.takes_clicks());
        assert!(fake.overlay.is_some());
    }

    #[test]
    fn a_proxy_that_can_be_neither_hidden_nor_emptied_is_stranded() {
        let (mut line, mut fake) = at_a();
        (fake.refuse_proxy_hides, fake.refuse_clears) = (true, true);
        assert_eq!(
            run_out(&mut line, &mut fake),
            Attempt::Stranded("clear refused")
        );
        assert!(fake.takes_clicks(), "only quitting removes this band");
    }

    #[test]
    fn running_out_with_the_surfaces_together_keeps_the_dock_where_it_was() {
        let (a, b, c) = (placement(0, 0), placement(400, 2), placement(800, 5));
        let (mut line, mut fake) = at_a();
        fake.refuse_uploads = vec![true; 3];
        line.request(b);
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert!(failed([true, false])(&line.attempt(&mut fake)));
        assert!(failed([false, false])(&line.attempt(&mut fake)));
        assert!(fake.holds(a) && line.contains([1, 1]));
        assert_eq!(line.attempt(&mut fake), Attempt::Idle);
        line.request(c);
        assert_eq!(line.attempt(&mut fake), Attempt::Placed(c));
        assert!(fake.holds(c));
    }

    /// The dock's retry on a fake desktop, driven by real frame callbacks.
    struct Probe {
        line: Line,
        fake: Fake,
        attempts: usize,
    }

    impl Probe {
        fn step(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
            self.attempts += 1;
            if let Attempt::Failed { retry: true, .. } = self.line.attempt(&mut self.fake) {
                on_next_frame(window, cx.weak_entity(), Self::step);
            }
        }
    }

    /// The window's root view; the probe is a separate entity, as the presence is to the dock.
    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
            div()
        }
    }

    /// Requests B with uploads refused per `refuse`, runs frames until none is pending, and
    /// returns the attempts, whether Windows holds B, and how many frames ran a retry.
    fn retry(cx: &mut TestAppContext, refuse: Vec<bool>) -> (usize, bool, usize) {
        let probe = cx.new(|_| Probe {
            line: Line::default(),
            fake: Fake {
                refuse_uploads: refuse,
                ..Fake::default()
            },
            attempts: 0,
        });
        let window = cx
            .update(|cx| cx.open_window(WindowOptions::default(), |_, cx| cx.new(|_| Blank)))
            .expect("a window");
        window
            .update(cx, |_, window, cx| {
                probe.update(cx, |probe, cx| {
                    probe.line.request(placement(400, 2));
                    probe.step(window, cx);
                });
            })
            .expect("the window");
        let mut frames = 0;
        while window
            .update(cx, |_, window, cx| window.simulate_next_frame(cx))
            .expect("the window")
            > 0
        {
            frames += 1;
        }
        probe.read_with(cx, |probe, _| {
            (probe.attempts, probe.fake.holds(placement(400, 2)), frames)
        })
    }

    #[gpui::test]
    fn two_failures_then_a_success_place_the_dock_through_frame_callbacks(cx: &mut TestAppContext) {
        assert_eq!(retry(cx, vec![true, true]), (3, true, 2));
    }

    #[gpui::test]
    fn three_failures_stop_retrying(cx: &mut TestAppContext) {
        assert_eq!(retry(cx, vec![true, true, true]), (3, false, 2));
    }
}
