//! The presence's one outline: the resting pill, joined to a round bulb while it stretches. The
//! dock paints it row by row and the hit mask owns the same rows, so a drawn pixel is a hit pixel.

/// A shape in physical screen pixels. A pixel belongs to it when the pixel's center does.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Outline {
    /// The resting pill, `[x, y, width, height]`, at least as wide as it is tall.
    pub(crate) pill: [f64; 4],
    /// The bulb's center. Inside the pill, the outline is just the pill.
    pub(crate) bulb: [f64; 2],
    pub(crate) bulb_radius: f64,
}

impl Outline {
    /// The pill's horizontal spine, between its two end-cap centers, and the caps' radius.
    fn spine(&self) -> ([f64; 2], [f64; 2], f64) {
        let [x, y, width, height] = self.pill;
        let radius = height / 2.0;
        let right = (x + width - radius).max(x + radius);
        ([x + radius, y + radius], [right, y + radius], radius)
    }

    /// The spine point nearest `point`, where the body grows from.
    pub(crate) fn anchor(&self, [x, _]: [f64; 2]) -> [f64; 2] {
        let (left, right, _) = self.spine();
        [x.max(left[0]).min(right[0]), left[1]]
    }

    /// Every pixel that can be inside, `[x, y, width, height]`.
    pub(crate) fn bounds(&self) -> [i32; 4] {
        let [x, y, width, height] = self.pill;
        let [bulb_x, bulb_y] = self.bulb;
        let radius = self.bulb_radius;
        let left = pixel(x.min(bulb_x - radius).floor());
        let top = pixel(y.min(bulb_y - radius).floor());
        let right = pixel((x + width).max(bulb_x + radius).ceil()) + 1;
        let bottom = pixel((y + height).max(bulb_y + radius).ceil()) + 1;
        [left, top, right - left, bottom - top]
    }

    /// The pixels of row `y` inside the outline, as `[left, right)`.
    pub(crate) fn row_span(&self, y: i32) -> Option<[i32; 2]> {
        let center = f64::from(y) + 0.5;
        let (left, right, radius) = self.spine();
        let pill = hull_extent((left, radius), (right, radius), center);
        // The body starts as the anchor's cap disc, which lies in the pill and crosses every
        // row the pill does, so where both meet a row their extents overlap into one span.
        let body = hull_extent(
            (self.anchor(self.bulb), radius),
            (self.bulb, self.bulb_radius),
            center,
        );
        let (low, high) = match (pill, body) {
            (Some(pill), Some(body)) => (pill.0.min(body.0), pill.1.max(body.1)),
            (Some(extent), None) | (None, Some(extent)) => extent,
            (None, None) => return None,
        };
        let left = pixel((low - 0.5).ceil());
        let right = pixel((high - 0.5).floor()) + 1;
        (right > left).then_some([left, right])
    }

    #[cfg(test)]
    fn contains(&self, [x, y]: [i32; 2]) -> bool {
        self.row_span(y)
            .is_some_and(|[left, right]| (left..right).contains(&x))
    }

    /// Whether both outlines cover exactly the same pixels, so a redraw would change nothing.
    pub(crate) fn same_pixels(&self, other: &Self) -> bool {
        let [_, top, _, height] = self.bounds();
        let [_, other_top, _, other_height] = other.bounds();
        (top.min(other_top)..(top + height).max(other_top + other_height))
            .all(|y| self.row_span(y) == other.row_span(y))
    }

    /// The same outline `by` pixels thinner on every side, for the fill inside its border.
    pub(crate) fn inset(&self, by: f64) -> Self {
        let [x, y, width, height] = self.pill;
        Self {
            pill: [x + by, y + by, width - 2.0 * by, height - 2.0 * by],
            bulb: self.bulb,
            bulb_radius: self.bulb_radius - by,
        }
    }
}

/// What the hit mask owns: the outline drawn now and, for one tick after the outline changes,
/// the one drawn before. A retracting edge then stays owned until a frame without it has had a
/// tick to reach the screen, and the tick after motion stops trims the mask to the outline.
// shortcut: there's no display-completion guarantee, so a frame presented more than one tick late can still expose a retracting edge; sync to GPUI present feedback if receipts show it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Ownership {
    /// The outline the dock draws.
    pub(crate) current: Outline,
    previous: Option<Outline>,
}

impl Ownership {
    /// Owns just `outline`.
    pub(crate) fn only(outline: Outline) -> Self {
        Self {
            current: outline,
            previous: None,
        }
    }

    /// What to own on the next tick: `next`, plus this tick's outline if `next` differs. A
    /// moved pill is a jump to a new position, not motion, so it carries nothing over and the
    /// mask never has to span two placements.
    #[must_use]
    #[expect(
        clippy::float_cmp,
        reason = "pills come from the integer layout, so one placement is always bitwise equal"
    )]
    pub(crate) fn then(self, next: Outline) -> Self {
        let moving = next != self.current && next.pill == self.current.pill;
        Self {
            current: next,
            previous: moving.then_some(self.current),
        }
    }

    /// Whether the mask owns exactly the drawn outline.
    pub(crate) fn trimmed(&self) -> bool {
        self.previous.is_none()
    }

    /// Every pixel that can be owned, `[x, y, width, height]`.
    pub(crate) fn bounds(&self) -> [i32; 4] {
        let [x, y, width, height] = self.current.bounds();
        let Some(previous) = self.previous else {
            return [x, y, width, height];
        };
        let [px, py, pw, ph] = previous.bounds();
        let (left, top) = (x.min(px), y.min(py));
        let (right, bottom) = ((x + width).max(px + pw), (y + height).max(py + ph));
        [left, top, right - left, bottom - top]
    }

    /// Row `y`'s owned pixels as up to two `[left, right)` spans, merged when they touch.
    pub(crate) fn row_spans(&self, y: i32) -> [Option<[i32; 2]>; 2] {
        let current = self.current.row_span(y);
        let previous = self.previous.and_then(|previous| previous.row_span(y));
        match (current, previous) {
            (Some([left, right]), Some([other_left, other_right]))
                if left <= other_right && other_left <= right =>
            {
                [Some([left.min(other_left), right.max(other_right)]), None]
            }
            spans => [spans.0, spans.1],
        }
    }

    pub(crate) fn contains(&self, [x, y]: [i32; 2]) -> bool {
        self.row_spans(y)
            .into_iter()
            .flatten()
            .any(|[left, right]| (left..right).contains(&x))
    }
}

/// Where the hit proxy sits, `[x, y, width, height]`: where it is while `bounds` stays inside,
/// otherwise `bounds` grown by `margin` on every side. So it moves only at a margin crossing.
pub(crate) fn retained(current: Option<[i32; 4]>, bounds: [i32; 4], margin: i32) -> [i32; 4] {
    let [x, y, width, height] = bounds;
    match current {
        Some([left, top, held_width, held_height])
            if x >= left
                && y >= top
                && x + width <= left + held_width
                && y + height <= top + held_height =>
        {
            [left, top, held_width, held_height]
        }
        _ => [
            x - margin,
            y - margin,
            width + 2 * margin,
            height + 2 * margin,
        ],
    }
}

/// The horizontal extent at height `y` of the convex hull of two discs, `(center, radius)`.
fn hull_extent(
    (a, a_radius): ([f64; 2], f64),
    (b, b_radius): ([f64; 2], f64),
    y: f64,
) -> Option<(f64, f64)> {
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    let mut include = |x: f64| {
        low = low.min(x);
        high = high.max(x);
    };
    for ([center_x, center_y], radius) in [(a, a_radius), (b, b_radius)] {
        let dy = y - center_y;
        if dy.abs() <= radius {
            let half = (radius * radius - dy * dy).sqrt();
            include(center_x - half);
            include(center_x + half);
        }
    }
    // Between the discs the hull is the quad of their outer tangent points; a convex shape
    // meets a row in one interval, so the parts' extreme crossings bound it.
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let distance = dx.hypot(dy);
    if distance > (a_radius - b_radius).abs() {
        let cos = (a_radius - b_radius) / distance;
        let sin = (1.0 - cos * cos).sqrt();
        let (ux, uy) = (dx / distance, dy / distance);
        let side = |sign: f64| [ux * cos - uy * sin * sign, uy * cos + ux * sin * sign];
        let along = |[cx, cy]: [f64; 2], [nx, ny]: [f64; 2], radius: f64| {
            [cx + nx * radius, cy + ny * radius]
        };
        let (plus, minus) = (side(1.0), side(-1.0));
        let quad = [
            along(a, plus, a_radius),
            along(b, plus, b_radius),
            along(b, minus, b_radius),
            along(a, minus, a_radius),
        ];
        for (index, start) in quad.iter().enumerate() {
            let end = quad[(index + 1) % quad.len()];
            let (top, bottom) = if start[1] <= end[1] {
                (*start, end)
            } else {
                (end, *start)
            };
            if top[1] <= y && y <= bottom[1] && bottom[1] > top[1] {
                include(top[0] + (y - top[1]) / (bottom[1] - top[1]) * (bottom[0] - top[0]));
            }
        }
    }
    low.is_finite().then_some((low, high))
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "already floored or ceiled screen coordinates, far inside i32"
)]
fn pixel(value: f64) -> i32 {
    value as i32
}

#[cfg(test)]
mod tests {
    use super::{Outline, Ownership, retained};

    /// A 104 x 40 pill at the origin, with its bulb at `bulb`.
    fn outline(bulb: [f64; 2]) -> Outline {
        Outline {
            pill: [0.0, 0.0, 104.0, 40.0],
            bulb,
            bulb_radius: 14.0,
        }
    }

    #[test]
    fn at_rest_the_outline_is_exactly_the_pill() {
        let resting = outline([52.0, 20.0]);
        assert_eq!(resting.row_span(20), Some([0, 104]));
        // Row 0's centers sit 19.5 px above the spine: the caps' chords end 4.44 px in.
        assert_eq!(resting.row_span(0), Some([16, 88]));
        assert_eq!(resting.row_span(39), Some([16, 88]));
        assert_eq!(resting.row_span(-1), None);
        assert_eq!(resting.row_span(40), None);
        assert!(resting.contains([52, 0]), "the top edge's middle is inside");
        assert!(!resting.contains([0, 0]), "the rounded corner is outside");
        assert!(!resting.contains([104, 20]), "the right edge is half-open");
        assert_eq!(resting.bounds(), [0, 0, 105, 41]);
    }

    #[test]
    fn a_stretched_body_owns_points_beyond_the_resting_pill() {
        let stretched = outline([52.0, 120.0]);
        assert!(stretched.contains([52, 120]), "the bulb's center");
        assert!(
            stretched.contains([52, 70]),
            "the neck between pill and bulb"
        );
        assert!(!stretched.contains([20, 120]), "beside the bulb");
        assert!(!stretched.contains([52, 135]), "below the bulb");
        assert_eq!(stretched.row_span(120), Some([38, 66]));
        assert_eq!(stretched.bounds(), [0, 0, 105, 135]);
    }

    #[test]
    fn a_sideways_stretch_joins_the_pill_in_one_span() {
        let stretched = outline([204.0, 20.0]);
        assert_eq!(stretched.row_span(20), Some([0, 218]));
        assert!(stretched.contains([150, 20]), "the neck beyond the cap");
        assert!(!stretched.contains([150, 2]), "above the narrowing neck");
    }

    #[test]
    fn the_inset_outline_leaves_a_one_pixel_border() {
        let resting = outline([52.0, 20.0]);
        assert_eq!(resting.inset(1.0).row_span(20), Some([1, 103]));
        assert_eq!(resting.inset(1.0).row_span(0), None);
    }

    #[test]
    fn the_proxy_moves_only_when_the_outline_leaves_its_margin() {
        let first = retained(None, [100, 50, 104, 40], 24);
        assert_eq!(first, [76, 26, 152, 88]);
        assert_eq!(retained(Some(first), [90, 40, 120, 60], 24), first);
        assert_eq!(retained(Some(first), [76, 26, 152, 88], 24), first);
        assert_eq!(
            retained(Some(first), [100, 50, 104, 120], 24),
            [76, 26, 152, 168]
        );
    }

    #[test]
    fn a_retracting_band_stays_owned_for_one_tick_then_is_trimmed() {
        let stretched = outline([52.0, 120.0]);
        let resting = outline([52.0, 20.0]);
        let retracting = Ownership::only(stretched).then(resting);
        assert_eq!(retracting.current, resting);
        assert!(!retracting.trimmed());
        assert_eq!(retracting.row_spans(120), [None, Some([38, 66])]);
        assert!(
            retracting.contains([52, 120]),
            "the bulb's old pixels are still owned"
        );
        assert_eq!(retracting.bounds(), [0, 0, 105, 135]);
        let trimmed = retracting.then(resting);
        assert!(trimmed.trimmed());
        assert!(!trimmed.contains([52, 120]), "the next tick lets them go");
        assert_eq!(trimmed.bounds(), [0, 0, 105, 41]);
    }

    #[test]
    fn touching_spans_merge_and_apart_spans_stay_apart() {
        let moved = Ownership::only(outline([52.0, 120.0])).then(outline([60.0, 120.0]));
        assert_eq!(moved.row_spans(120), [Some([38, 74]), None]);
        let jumped = Ownership::only(outline([52.0, 120.0])).then(outline([152.0, 120.0]));
        assert_eq!(jumped.row_spans(120), [Some([135, 166]), Some([38, 66])]);
        assert!(
            !jumped.contains([100, 120]),
            "the gap between them passes through"
        );
    }

    #[test]
    fn a_hidden_or_subpixel_bulb_move_changes_no_pixel() {
        let resting = outline([52.0, 20.0]);
        assert!(
            resting.same_pixels(&outline([60.0, 22.0])),
            "the bulb hides in the pill"
        );
        assert!(!resting.same_pixels(&outline([52.0, 120.0])));
        let stretched = outline([52.0, 120.0]);
        assert!(stretched.same_pixels(&outline([52.0, 120.01])));
        assert!(!stretched.same_pixels(&outline([52.0, 122.0])));
    }

    #[test]
    fn a_new_position_owns_only_the_new_placement() {
        let stretched = outline([52.0, 120.0]);
        let elsewhere = Outline {
            pill: [2000.0, 900.0, 104.0, 40.0],
            bulb: [2052.0, 920.0],
            bulb_radius: 14.0,
        };
        let jumped = Ownership::only(stretched).then(elsewhere);
        assert!(jumped.trimmed());
        assert!(
            !jumped.contains([52, 120]),
            "the old placement passes through at once"
        );
        assert_eq!(jumped.bounds(), [2000, 900, 105, 41]);
    }

    #[test]
    fn an_unchanged_outline_owns_nothing_extra() {
        let resting = outline([52.0, 20.0]);
        assert_eq!(
            Ownership::only(resting).then(resting),
            Ownership::only(resting)
        );
    }
}
