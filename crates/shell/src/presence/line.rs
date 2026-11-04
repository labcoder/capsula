//! The dock at rest: a short line near the work area's edge. GPUI draws it as an antialiased
//! rounded rect, and the hit proxy owns it plus a trigger band above and below, so the pointer
//! finds it without the line growing. A placement becomes current only once Windows accepts its
//! mask, so the line never shows where the mask isn't.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    Context, IntoElement, ParentElement as _, Render, Styled as _, Window, canvas, div, px,
};

use super::diagnostics::{self, PendingPaint};
use super::observer::{self, Drawn, Surface};
use super::paint::Stamp;
use super::{Presence, platform};
use crate::platform::{MaskUpdate, PresenceError};

/// The line's size, in logical pixels.
const LINE: [f32; 2] = [56.0, 3.0];
/// How far above and below the line the pointer still finds it, in logical pixels.
const TRIGGER: f32 = 8.0;
/// Failed uploads in a row before the line stops retrying; a position choice tries again.
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
    /// `paint` grown by the trigger band above and below, rounded outward. Only these pixels
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
        let hit_top = whole((y - trigger).floor()).min(top);
        let hit_bottom = whole((y + height + trigger).ceil()).max(bottom);
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
    pub(super) fn row_spans(&self, y: i32) -> [Option<[i32; 2]>; 2] {
        let [left, top, width, height] = self.hit;
        [
            (top..top + height)
                .contains(&y)
                .then_some([left, left + width]),
            None,
        ]
    }

    pub(super) fn contains(&self, [x, y]: [i32; 2]) -> bool {
        let [left, top, width, height] = self.hit;
        (left..left + width).contains(&x) && (top..top + height).contains(&y)
    }
}

/// The line as far as Windows accepted it, and the uploads that failed since.
#[derive(Debug, Default)]
pub(super) struct Line {
    accepted: Option<LineFrame>,
    failed: u8,
}

impl Line {
    /// What the mask owns now: the pixels that take the pointer.
    pub(super) fn contains(&self, point: [i32; 2]) -> bool {
        self.accepted.is_some_and(|frame| frame.contains(point))
    }

    /// Records one upload of `frame`. Only an accepted one becomes current.
    fn record(&mut self, frame: LineFrame, upload: &Result<MaskUpdate, PresenceError>) {
        if upload.is_ok() {
            self.accepted = Some(frame);
            self.failed = 0;
        } else {
            self.failed = self.failed.saturating_add(1);
        }
    }
}

impl Presence {
    /// Puts the line in the dock's slot: uploads its hit band, and once Windows accepts it,
    /// draws it there. A failed upload changes nothing on screen and is retried on the next
    /// frame, up to [`MAX_FAILED_UPLOADS`] times in a row.
    pub(super) fn place_line(&mut self, cx: &mut Context<'_, Self>) {
        let frame = LineFrame::in_slot(self.layout.dock, self.scale);
        let begin_qpc = observer::begin(cx);
        let update = self.native.reshape(frame.hit, |y| frame.row_spans(y));
        let observed = observer::commit(cx, begin_qpc, &frame, frame.hit, &update);
        let before = self.line.accepted;
        self.line.record(frame, &update);
        if let Err(error) = &update {
            if self.line.failed == 1 {
                tracing::warn!(%error, "can't update the hit mask");
            }
            if self.line.failed < MAX_FAILED_UPLOADS {
                let owner = cx.weak_entity();
                let scheduled = self.dock_window.update(cx, |_, window, _| {
                    window.on_next_frame(move |_, cx| {
                        if owner.update(cx, Self::place_line).is_err() {
                            tracing::debug!("the presence closed before the line was placed");
                        }
                    });
                });
                if let Err(error) = scheduled {
                    tracing::warn!(%error, "can't retry the hit mask");
                }
            }
            return;
        }
        let origin = [self.layout.overlay[0], self.layout.overlay[1]];
        self.dock.update(cx, |dock, cx| {
            if dock.paint != Some(frame.paint) || dock.origin != origin {
                dock.paint = Some(frame.paint);
                dock.origin = origin;
                dock.drawn = observed;
                cx.notify();
            }
        });
        // The mask can change under a still pointer, which Windows may never report.
        let crossed = before != self.line.accepted
            && platform::cursor_position().is_ok_and(|point| {
                before.is_some_and(|frame| frame.contains(point)) != self.line.contains(point)
            });
        if crossed && let Err(error) = self.pointer_changed(cx) {
            tracing::debug!(%error, "can't recheck hover after the mask changed");
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
        div()
            .size_full()
            .relative()
            .child(Stamp(Surface::Dock, self.drawn))
            .children(line)
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
    use super::{Line, LineFrame, slot};
    use crate::platform::{MaskUpdate, PresenceError};

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
        let frame = LineFrame::at([100.3, 20.6], 1.25);
        assert_eq!(frame.paint, [100, 21, 70, 3]);
        assert_eq!(frame.hit, [100, 10, 70, 25]);
        assert_edges(&frame, [100, 10, 169, 34]);
        assert_eq!(frame.row_spans(10), [Some([100, 170]), None]);
        assert_eq!(frame.row_spans(9), [None, None]);
        assert_eq!(frame.row_spans(35), [None, None]);
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
    fn the_line_sits_centred_in_its_slot_with_its_band_inside() {
        assert_eq!(slot(1.0), [58, 21]);
        let frame = LineFrame::in_slot([0, 0, 58, 21], 1.0);
        assert_eq!(frame.paint, [1, 9, 56, 3]);
        assert_eq!(frame.hit, [1, 1, 56, 19]);
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let [width, height] = slot(scale);
            let [x, y, hit_width, hit_height] =
                LineFrame::in_slot([5, 7, width, height], scale).hit;
            assert!(
                x >= 5 && y >= 7 && x + hit_width <= 5 + width && y + hit_height <= 7 + height,
                "{scale}"
            );
        }
    }

    #[test]
    fn only_an_accepted_upload_moves_the_line() {
        let first = LineFrame::in_slot([0, 0, 58, 21], 1.0);
        let moved = LineFrame::in_slot([400, 0, 58, 21], 1.0);
        let mut line = Line::default();
        assert!(
            !line.contains([20, 10]),
            "nothing is owned before an upload"
        );
        line.record(first, &Ok(MaskUpdate::Moved));
        assert!(line.contains([20, 10]));
        line.record(moved, &Err(PresenceError::MaskSize));
        assert_eq!((line.accepted, line.failed), (Some(first), 1));
        line.record(moved, &Ok(MaskUpdate::Unchanged));
        assert_eq!((line.accepted, line.failed), (Some(moved), 0));
        assert!(!line.contains([20, 10]) && line.contains([420, 10]));
    }
}
