//! The dock at rest: a short line near the work area's edge. GPUI draws it as an antialiased
//! rounded rect, and the hit proxy owns it plus a trigger band above and below, so the pointer
//! finds it without the line growing. A placement becomes current only once Windows accepts its
//! mask, so the line never shows where the mask isn't.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    App, Context, IntoElement, ParentElement as _, Render, Styled as _, WeakEntity, Window, canvas,
    div, px,
};

use super::diagnostics::{self, PendingPaint};
use super::geometry::Layout;
use super::observer::{self, Drawn, Surface};
use super::paint::Stamp;
use super::{OpenError, Presence, platform};

/// The line's size, in logical pixels.
const LINE: [f32; 2] = [56.0, 3.0];
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

/// One placement of the dock: its layout, the line in its slot and the tray's position entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Placement {
    pub(super) layout: Layout,
    pub(super) frame: LineFrame,
    pub(super) index: usize,
}

/// What one attempt at the requested placement did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Attempt<E> {
    /// Nothing is requested, or its attempts ran out.
    Idle,
    /// The mask and the overlay took it; it's current now.
    Placed(Placement),
    /// It failed and nothing moved. `retry` says whether an attempt is left.
    Failed { error: E, first: bool, retry: bool },
}

/// The dock's placement as Windows accepted it, and the one asked for since.
#[derive(Debug, Default)]
pub(super) struct Line {
    /// Owned by the mask and drawn, with the overlay at its layout.
    accepted: Option<Placement>,
    /// Asked for and not yet accepted.
    requested: Option<Placement>,
    /// Failed attempts at `requested` in a row.
    failed: u8,
}

impl Line {
    /// What the mask owns now: the pixels that take the pointer.
    pub(super) fn contains(&self, point: [i32; 2]) -> bool {
        self.accepted
            .is_some_and(|placement| placement.frame.contains(point))
    }

    pub(super) fn accepted(&self) -> Option<Placement> {
        self.accepted
    }

    /// A new choice replaces any pending one and gets a fresh run of attempts.
    pub(super) fn request(&mut self, placement: Placement) {
        self.requested = Some(placement);
        self.failed = 0;
    }

    /// Tries the requested placement with `place`, which must leave the mask and the overlay
    /// as they were when it fails. Only success makes the placement current.
    pub(super) fn attempt<E>(
        &mut self,
        place: impl FnOnce(Placement) -> Result<(), E>,
    ) -> Attempt<E> {
        let Some(placement) = self.requested.filter(|_| self.failed < MAX_FAILED_UPLOADS) else {
            return Attempt::Idle;
        };
        match place(placement) {
            Ok(()) => {
                self.accepted = self.requested.take();
                self.failed = 0;
                Attempt::Placed(placement)
            }
            Err(error) => {
                self.failed += 1;
                Attempt::Failed {
                    error,
                    first: self.failed == 1,
                    retry: self.failed < MAX_FAILED_UPLOADS,
                }
            }
        }
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

impl Presence {
    /// Asks for the dock at `placement` and tries it now. Returns whether it's current; if
    /// not, the dock stays where it was and the next frames retry.
    ///
    /// # Errors
    /// Fails when the dock window is gone.
    pub(super) fn request_line(
        &mut self,
        placement: Placement,
        cx: &mut Context<'_, Self>,
    ) -> Result<bool, OpenError> {
        self.line.request(placement);
        let owner = cx.weak_entity();
        let handle = self.dock_window;
        handle
            .update(cx, |_, window, cx| self.try_line(owner, window, cx))
            .map_err(|error| OpenError::Gpui(error.into()))
    }

    fn retry_line(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.try_line(cx.weak_entity(), window, cx) {
            return;
        }
        if let Some(placement) = self.line.accepted()
            && let Err(error) = self.native.show(placement.index)
        {
            tracing::warn!(%error, "can't show the dock's proxy");
        }
        if let Err(error) = self.pointer_changed(cx) {
            tracing::debug!(%error, "can't recheck hover after the dock moved");
        }
    }

    /// Uploads the requested band's mask, then moves the overlay to it. Only both together make
    /// it current, with its layout and line published; otherwise the mask is put back, nothing
    /// moves, and the next frame tries again while attempts are left.
    fn try_line(&mut self, owner: WeakEntity<Self>, window: &mut Window, cx: &mut App) -> bool {
        let accepted = self.line.accepted().map(|placement| placement.frame);
        let native = &mut self.native;
        let mut drawn = Drawn::default();
        let attempt = self.line.attempt(|placement| {
            let frame = placement.frame;
            let begin_qpc = observer::begin(cx);
            let update = native.reshape(frame.hit, |y| frame.row_spans(y));
            drawn = observer::commit(cx, begin_qpc, Some(&frame), frame.hit, &update);
            update?;
            if let Err(error) = platform::place_presence_window(window, placement.layout.overlay) {
                let proxy = accepted.map_or(frame.hit, |frame| frame.hit);
                let begin_qpc = observer::begin(cx);
                let update = native.reshape(proxy, |y| {
                    accepted.map_or([None; 2], |frame| frame.row_spans(y))
                });
                observer::commit(cx, begin_qpc, accepted.as_ref(), proxy, &update);
                if let Err(restore) = update {
                    tracing::warn!(%restore, "can't put the hit mask back under the line");
                }
                return Err(error);
            }
            window.bounds_changed(cx);
            Ok(())
        });
        match attempt {
            Attempt::Idle => false,
            Attempt::Placed(placement) => {
                self.layout = placement.layout;
                let origin = [placement.layout.overlay[0], placement.layout.overlay[1]];
                self.dock.update(cx, |dock, cx| {
                    dock.paint = Some(placement.frame.paint);
                    dock.origin = origin;
                    dock.drawn = drawn;
                    cx.notify();
                });
                true
            }
            Attempt::Failed {
                error,
                first,
                retry,
            } => {
                if first {
                    tracing::warn!(%error, "can't place the dock");
                }
                if retry {
                    on_next_frame(window, owner, Self::retry_line);
                }
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
    use gpui::TestAppContext;
    use gpui_kit::{AppContext as _, Context, IntoElement, Render, Window, WindowOptions, div};

    use super::{Attempt, Line, LineFrame, Placement, on_next_frame, slot};
    use crate::presence::geometry::Layout;

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
        assert_eq!(frame.row_spans(11), [Some([100, 170]), None]);
        assert_eq!(frame.row_spans(10), [None, None]);
        assert_eq!(frame.row_spans(34), [None, None]);
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

    #[test]
    fn a_failed_jump_leaves_the_overlay_and_the_owned_band_where_they_were() {
        let (a, b) = (placement(0, 0), placement(400, 2));
        let mut line = Line::default();
        line.request(a);
        assert_eq!(line.attempt(|_| Ok::<_, ()>(())), Attempt::Placed(a));
        line.request(b);
        let mut tried = None;
        let failed = line.attempt(|placement| {
            tried = Some(placement);
            Err("refused")
        });
        assert_eq!(
            failed,
            Attempt::Failed {
                error: "refused",
                first: true,
                retry: true
            }
        );
        assert_eq!(tried, Some(b));
        let current = line.accepted().expect("A stays current");
        assert_eq!((current.layout.overlay, current.index), ([0, 0, 58, 21], 0));
        assert!(line.contains([1, 1]) && line.contains([56, 19]));
        assert!(!line.contains([401, 1]) && !line.contains([456, 19]));
        assert_eq!(line.attempt(|_| Ok::<_, ()>(())), Attempt::Placed(b));
        let current = line.accepted().expect("B is current");
        assert_eq!(
            (current.layout.overlay, current.index),
            ([400, 0, 58, 21], 2)
        );
        assert!(line.contains([401, 1]) && line.contains([456, 19]));
        assert!(!line.contains([1, 1]));
    }

    #[test]
    fn a_new_choice_replaces_a_failing_one_and_gets_a_fresh_run() {
        let (a, b, c) = (placement(0, 0), placement(400, 2), placement(800, 5));
        let mut line = Line::default();
        line.request(a);
        assert_eq!(line.attempt(|_| Ok::<_, ()>(())), Attempt::Placed(a));
        line.request(b);
        let retries: Vec<_> = (0..3)
            .map(|_| match line.attempt(|_| Err(())) {
                Attempt::Failed { first, retry, .. } => (first, retry),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(retries, [(true, true), (false, true), (false, false)]);
        assert_eq!(
            line.attempt(|_| -> Result<(), ()> { panic!("no attempts left") }),
            Attempt::Idle
        );
        assert_eq!(line.accepted(), Some(a));
        line.request(c);
        let mut tried = None;
        assert_eq!(
            line.attempt(|placement| {
                tried = Some(placement);
                Ok::<_, ()>(())
            }),
            Attempt::Placed(c)
        );
        assert_eq!((tried, line.accepted()), (Some(c), Some(c)));
        assert!(line.contains([801, 1]) && !line.contains([401, 1]));
    }

    /// The dock's retry without the native mask: each attempt takes the next scripted result.
    struct Probe {
        line: Line,
        results: Vec<Result<(), &'static str>>,
        tried: usize,
        overlay: Option<[i32; 4]>,
    }

    impl Probe {
        fn step(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
            let results = &mut self.results;
            let tried = &mut self.tried;
            match self.line.attempt(|_| {
                *tried += 1;
                results.remove(0)
            }) {
                Attempt::Placed(placement) => self.overlay = Some(placement.layout.overlay),
                Attempt::Failed { retry: true, .. } => {
                    on_next_frame(window, cx.weak_entity(), Self::step);
                }
                Attempt::Failed { .. } | Attempt::Idle => {}
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

    /// Requests B and runs frames until none is pending; returns the attempts, the overlay and
    /// how many frames ran a retry.
    fn retry(
        cx: &mut TestAppContext,
        results: Vec<Result<(), &'static str>>,
    ) -> (usize, Option<[i32; 4]>, usize) {
        let probe = cx.new(|_| Probe {
            line: Line::default(),
            results,
            tried: 0,
            overlay: None,
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
        probe.read_with(cx, |probe, _| (probe.tried, probe.overlay, frames))
    }

    #[gpui::test]
    fn two_failures_then_a_success_place_the_dock_through_frame_callbacks(cx: &mut TestAppContext) {
        assert_eq!(
            retry(cx, vec![Err("refused"), Err("refused"), Ok(())]),
            (3, Some([400, 0, 58, 21]), 2)
        );
    }

    #[gpui::test]
    fn three_failures_stop_retrying(cx: &mut TestAppContext) {
        assert_eq!(
            retry(cx, vec![Err("refused"), Err("refused"), Err("refused")]),
            (3, None, 2)
        );
    }
}
