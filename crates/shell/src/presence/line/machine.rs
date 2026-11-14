//! The dock line's placement, apart from how it is drawn: where the line and its band go, and
//! the attempts that bring the native surfaces into line with a requested placement.

use crate::presence::geometry::Layout;

#[cfg(test)]
pub(crate) mod fixture;

/// The most owned `[left, right)` column spans one hit-mask row holds: two shapes, each with
/// its current and previous snapshot.
pub(crate) const ROW_SPANS: usize = 4;
/// One screen row's owned columns.
pub(crate) type RowSpans = [Option<[i32; 2]>; ROW_SPANS];

/// The line's size, in logical pixels.
const LINE: [f32; 2] = [56.0, 3.0];
/// How far above and below the line the pointer still finds it, in logical pixels.
const TRIGGER: f32 = 8.0;
/// Failed attempts in a row before a placement stops retrying; a new position choice starts a
/// fresh run.
const MAX_FAILED_UPLOADS: u8 = 3;

/// The dock's slot, in physical pixels: the line and its trigger band with a pixel to spare on
/// every side for rounding.
pub(crate) fn slot(scale: f32) -> [i32; 2] {
    [LINE[0], LINE[1] + 2.0 * TRIGGER].map(|value| whole(f64::from(value * scale).ceil()) + 2)
}

/// One placement of the line. Both halves come from the same slot and publish together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LineFrame {
    /// The line's device pixels, `[x, y, width, height]`: each edge rounded half toward zero,
    /// as GPUI snaps the bounds it draws. Only these pixels are drawn.
    pub(crate) paint: [i32; 4],
    /// `paint` with its snapped edges grown by the trigger band above and below, rounded outward. Only these pixels
    /// are owned; every row holds the same span.
    pub(crate) hit: [i32; 4],
}

impl LineFrame {
    /// The line with its top-left at `origin`, in physical screen pixels, at `scale`.
    pub(crate) fn at([x, y]: [f64; 2], scale: f32) -> Self {
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
    pub(crate) fn in_slot([x, y, width, height]: [i32; 4], scale: f32) -> Self {
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
    pub(crate) fn row_spans(&self, y: i32) -> RowSpans {
        let [left, top, width, height] = self.hit;
        let mut spans = [None; ROW_SPANS];
        spans[0] = (top..top + height)
            .contains(&y)
            .then_some([left, left + width]);
        spans
    }

    pub(crate) fn contains(&self, [x, y]: [i32; 2]) -> bool {
        let [left, top, width, height] = self.hit;
        (left..left + width).contains(&x) && (top..top + height).contains(&y)
    }
}

/// One placement of the dock: its layout, the line in its slot and the tray's position entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    pub(crate) layout: Layout,
    pub(crate) frame: LineFrame,
    pub(crate) index: usize,
}

/// The native surfaces a placement must bring into line: the hit proxy's mask, the overlay
/// window and the proxy's visibility. Each call either does its whole change or none of it.
pub(crate) trait Surfaces {
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
pub(crate) enum Attempt<E> {
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
pub(crate) struct Line {
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
    pub(crate) fn position_index(&self) -> Option<usize> {
        self.requested
            .or(self.accepted)
            .map(|placement| placement.index)
    }

    /// The line Windows last accepted.
    pub(crate) fn accepted_frame(&self) -> Option<LineFrame> {
        self.accepted.map(|placement| placement.frame)
    }

    /// A grab was accepted: from now until the pull hands the dock back, it's drawn in this
    /// line's coordinates, so the line takes no attempt.
    pub(crate) fn pull_started(&mut self) {
        self.pulled = true;
    }

    /// A pull moved the overlay: the next attempt moves it back to the line's.
    pub(crate) fn pull_overlay(&mut self) {
        self.held.overlay = None;
        self.pulled = true;
    }

    /// Windows accepted a pull's mask over `area`: the next attempt gives the line its band
    /// back, and failing closed empties the pull's area.
    pub(crate) fn pull_mask(&mut self, area: [i32; 4]) {
        self.held.mask = None;
        self.held.proxy = Some(area);
        self.pulled = true;
    }

    /// The pull is done with the mask and the overlay, and the dock draws no picture of it:
    /// ask for the line again, the pending placement if there is one, with a fresh run of
    /// attempts.
    pub(crate) fn reclaim(&mut self) {
        self.pulled = false;
        self.requested = self.requested.or(self.accepted);
        self.failed = 0;
    }

    /// What the mask owns now: the pixels that take the pointer.
    pub(crate) fn contains(&self, point: [i32; 2]) -> bool {
        self.accepted
            .is_some_and(|placement| placement.frame.contains(point))
    }

    /// A new choice replaces any pending one and gets a fresh run of attempts.
    pub(crate) fn request(&mut self, placement: Placement) {
        self.requested = Some(placement);
        self.failed = 0;
    }

    /// Brings the surfaces to the requested placement, finishing from wherever an earlier
    /// attempt left them. It's current only once all three hold it. When the last attempt
    /// fails with the surfaces apart, the dock is hidden rather than left showing a line it
    /// doesn't own or owning a band it doesn't show.
    pub(crate) fn attempt<S: Surfaces>(&mut self, surfaces: &mut S) -> Attempt<S::Error> {
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
    use super::fixture::{Fake, at_a, failed, placement};
    use super::{Attempt, Line, LineFrame, ROW_SPANS, slot};

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
}
