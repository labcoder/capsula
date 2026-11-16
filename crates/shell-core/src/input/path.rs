//! The pointer's path over a screen hold: the samples between recognition and release, thinned
//! so that a hold of any length fits a [`PointerPath`].

use kumochi_protocol::geometry::{MAX_PATH_POINTS, PhysicalPoint, PhysicalRect, PointerPath};

/// How far the pointer must go from where it was at recognition before anything is recorded,
/// in logical pixels: a hand resting on the mouse draws nothing. Once it has gone that far the
/// path starts from where it was.
const DEAD_ZONE: f64 = 24.0;
/// The least distance between two kept points, in logical pixels, until the path first fills.
const SPACING: f64 = 2.0;

/// Records a hold's pointer path in physical desktop pixels. It has no `Debug`: where the user
/// pointed is theirs.
pub struct PathRecorder {
    origin: PhysicalPoint,
    /// The display the line is drawn on. A point anywhere else is not kept.
    display: PhysicalRect,
    /// The dead zone's radius, in physical pixels.
    dead_zone: f64,
    /// The least distance between kept points, in physical pixels. It doubles when the path
    /// fills.
    spacing: f64,
    points: Vec<PhysicalPoint>,
}

impl PathRecorder {
    /// A recorder for a hold recognized with the pointer at `origin`. `scale` is `display`'s
    /// physical pixels per logical pixel, and `display` is the one the line is drawn on.
    pub fn new(origin: PhysicalPoint, scale: f32, display: PhysicalRect) -> Self {
        // No display is under 100%, and a scale that isn't a number counts as 100%.
        let scale = f64::from(scale).max(1.0);
        Self {
            origin,
            display,
            dead_zone: DEAD_ZONE * scale,
            spacing: SPACING * scale,
            points: Vec::with_capacity(MAX_PATH_POINTS + 1),
        }
    }

    /// Adds a sample. True when the line changed and must be repainted.
    pub fn sample(&mut self, point: PhysicalPoint) -> bool {
        if !on(self.display, point) {
            return false;
        }
        let (from, least) = match self.points.last() {
            Some(last) => (*last, self.spacing),
            None => (self.origin, self.dead_zone),
        };
        if distance_squared(from, point) < least * least {
            return false;
        }
        // The line starts where the pointer was, when the display shows that place.
        if self.points.is_empty() && on(self.display, self.origin) {
            self.points.push(self.origin);
        }
        self.points.push(point);
        if self.points.len() > MAX_PATH_POINTS {
            // Every second point goes. The count is odd, so the first and the newest stay.
            let mut keep = false;
            self.points.retain(|_| {
                keep = !keep;
                keep
            });
            self.spacing *= 2.0;
        }
        true
    }

    /// The kept points, oldest first.
    pub fn points(&self) -> &[PhysicalPoint] {
        &self.points
    }

    /// The path a release carries: empty when the pointer never left the dead zone.
    pub fn finish(self) -> PointerPath {
        PointerPath(self.points)
    }
}

fn on(display: PhysicalRect, PhysicalPoint([x, y]): PhysicalPoint) -> bool {
    let within = |at: i32, origin: i32, size: u32| {
        (0..i64::from(size)).contains(&(i64::from(at) - i64::from(origin)))
    };
    within(x, display.origin[0], display.size[0]) && within(y, display.origin[1], display.size[1])
}

fn distance_squared(
    PhysicalPoint([ax, ay]): PhysicalPoint,
    PhysicalPoint([bx, by]): PhysicalPoint,
) -> f64 {
    let (dx, dy) = (f64::from(bx) - f64::from(ax), f64::from(by) - f64::from(ay));
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::marker::PhantomData;

    use super::*;

    const DISPLAY: PhysicalRect = PhysicalRect {
        origin: [-5_000, -500],
        size: [20_000, 1_000],
    };
    const ORIGIN: PhysicalPoint = PhysicalPoint([-4_000, 0]);

    fn at(dx: i32, dy: i32) -> PhysicalPoint {
        PhysicalPoint([ORIGIN.0[0] + dx, ORIGIN.0[1] + dy])
    }

    fn recorder(scale: f32) -> PathRecorder {
        PathRecorder::new(ORIGIN, scale, DISPLAY)
    }

    #[test]
    fn nothing_is_recorded_until_the_pointer_is_24_logical_pixels_away() {
        for (scale, edge) in [(1.0, 24), (1.5, 36)] {
            for [x, y] in [[1, 0], [-1, 0], [0, 1], [0, -1]] {
                let mut path = recorder(scale);
                assert!(!path.sample(ORIGIN), "{scale} {x} {y}");
                let under = at(x * (edge - 1), y * (edge - 1));
                assert!(!path.sample(under), "{scale} {x} {y}");
                assert!(path.points().is_empty(), "{scale} {x} {y}");
                let first = at(x * edge, y * edge);
                assert!(path.sample(first), "{scale} {x} {y}");
                assert_eq!(path.points(), [ORIGIN, first], "{scale} {x} {y}");
            }
        }
        // The distance is the straight one: 20 by 13 is under 24, and 17 by 17 is over.
        let mut path = recorder(1.0);
        assert!(!path.sample(at(20, 13)));
        assert!(path.sample(at(17, 17)));
    }

    #[test]
    fn a_sample_is_kept_at_two_logical_pixels_from_the_last_kept_one() {
        for (scale, edge, step) in [(1.0, 24, 2), (1.5, 36, 3)] {
            let mut path = recorder(scale);
            assert!(path.sample(at(edge, 0)), "{scale}");
            assert!(!path.sample(at(edge, step - 1)), "{scale}");
            // One short of the last kept point, then one step from it: the distance is not
            // taken from the sample before.
            assert!(!path.sample(at(edge + step - 1, 0)), "{scale}");
            assert!(path.sample(at(edge + step, 0)), "{scale}");
            // Once the line has started, the dead zone is over.
            assert!(path.sample(ORIGIN), "{scale}");
            assert_eq!(
                path.points(),
                [ORIGIN, at(edge, 0), at(edge + step, 0), ORIGIN],
                "{scale}"
            );
        }
    }

    #[test]
    fn a_full_path_halves_and_keeps_its_first_and_newest_points() {
        // The path in order: where the pointer was, then a sample every two pixels.
        let point = |index: usize| match i32::try_from(index).expect("small") {
            0 => ORIGIN,
            index => at(22 + 2 * index, 0),
        };
        let mut path = recorder(1.0);
        for index in 1..MAX_PATH_POINTS {
            assert!(path.sample(point(index)), "{index}");
        }
        assert_eq!(path.points().len(), MAX_PATH_POINTS);
        assert_eq!(path.points().last(), Some(&point(MAX_PATH_POINTS - 1)));
        assert!(path.sample(point(MAX_PATH_POINTS)));
        let kept: Vec<_> = (0..=MAX_PATH_POINTS).step_by(2).map(point).collect();
        assert_eq!(kept.len(), 1_025);
        assert_eq!(path.points().first(), Some(&ORIGIN));
        assert_eq!(path.points().last(), Some(&point(MAX_PATH_POINTS)));
        assert!(path.points() == kept, "every second point went");
        // The spacing doubled with it.
        let PhysicalPoint([last, _]) = point(MAX_PATH_POINTS);
        assert!(!path.sample(PhysicalPoint([last + 3, 0])));
        assert!(path.sample(PhysicalPoint([last + 4, 0])));
        assert_eq!(path.points().len(), 1_026);
    }

    #[test]
    fn a_hold_of_any_length_fits_a_path() {
        let mut path = recorder(1.0);
        for index in 0..60_000 {
            path.sample(at(24 + (index * 7) % 9_000, index % 400));
            assert!(path.points().len() <= MAX_PATH_POINTS, "{index}");
        }
        assert!(path.spacing >= 4.0, "the path filled at least once");
        assert!(path.points().len() > MAX_PATH_POINTS / 2);
        assert_eq!(path.points().first(), Some(&ORIGIN));
        assert_eq!(path.finish().check(), Ok(()));
    }

    #[test]
    fn only_points_on_the_display_are_kept() {
        let [left, top] = DISPLAY.origin;
        let (right, bottom) = (left + 20_000, top + 1_000);
        let mut path = recorder(1.0);
        for off in [
            [left - 1, 0],
            [right, 0],
            [0, top - 1],
            [0, bottom],
            [i32::MIN, i32::MAX],
            [i32::MAX, i32::MIN],
        ] {
            assert!(!path.sample(PhysicalPoint(off)), "{off:?}");
        }
        assert!(path.points().is_empty());
        let corners = [[left, top], [right - 1, bottom - 1]].map(PhysicalPoint);
        for corner in corners {
            assert!(path.sample(corner), "{corner:?}");
        }
        // A point off the display leaves a started line as it was.
        assert!(!path.sample(PhysicalPoint([right + 100, bottom + 100])));
        assert_eq!(
            path.finish(),
            PointerPath(vec![ORIGIN, corners[0], corners[1]])
        );
    }

    #[test]
    fn a_path_starts_where_the_pointer_was_only_when_the_display_shows_that_place() {
        let [left, _] = DISPLAY.origin;
        let mut path = PathRecorder::new(PhysicalPoint([left - 10, 0]), 1.0, DISPLAY);
        // On the display, but 13 pixels from the start: still resting.
        assert!(!path.sample(PhysicalPoint([left + 3, 0])));
        let first = PhysicalPoint([left + 14, 0]);
        assert!(path.sample(first));
        assert_eq!(path.points(), [first]);
    }

    #[test]
    fn a_pointer_that_stays_in_the_dead_zone_leaves_an_empty_path() {
        let mut path = recorder(1.0);
        for _ in 0..100 {
            assert!(!path.sample(ORIGIN));
            assert!(!path.sample(at(-23, 0)));
        }
        assert!(path.points().is_empty());
        assert_eq!(path.finish(), PointerPath(Vec::new()));
    }

    #[test]
    fn a_scale_under_one_or_not_a_number_counts_as_one() {
        for scale in [0.0, 0.5, -2.0, f32::NAN] {
            let mut path = recorder(scale);
            assert!(!path.sample(at(23, 0)), "{scale}");
            assert!(path.sample(at(24, 0)), "{scale}");
            assert!(!path.sample(at(25, 0)), "{scale}");
            assert!(path.sample(at(26, 0)), "{scale}");
        }
    }

    /// Whether `T` has a `Debug`: the inherent method is found only when it does.
    struct Probe<T>(PhantomData<T>);

    impl<T: fmt::Debug> Probe<T> {
        #[expect(clippy::unused_self, reason = "method lookup is the probe")]
        fn prints(&self) -> bool {
            true
        }
    }

    trait Silent {
        fn prints(&self) -> bool {
            false
        }
    }

    impl<T> Silent for Probe<T> {}

    #[test]
    fn nothing_that_holds_the_pointer_has_a_debug() {
        assert!(
            Probe::<PointerPath>(PhantomData).prints(),
            "the probe finds a Debug"
        );
        assert!(!Probe::<PathRecorder>(PhantomData).prints());
        assert!(!Probe::<crate::runs::HoldRelease>(PhantomData).prints());
        assert!(!Probe::<crate::runs::ScreenHold>(PhantomData).prints());
    }
}
