//! The pointer's path over a screen hold: the samples between recognition and release, eased
//! lightly and thinned so that a hold of any length fits a [`PointerPath`].

use std::time::{Duration, Instant};

use kumochi_protocol::geometry::{MAX_PATH_POINTS, PhysicalPoint, PhysicalRect, PointerPath};

/// How far the pointer must go from where it was at recognition before anything is recorded,
/// in logical pixels: a hand resting on the mouse draws nothing. Once it has gone that far the
/// path starts from where it was.
const DEAD_ZONE: f64 = 24.0;
/// The least distance between two kept points, in logical pixels.
const SPACING: f64 = 2.0;
/// The easing's time constant. A kept point is where a first-order lag this long behind the
/// pointer is, by the clock and not by the frame, so every frame rate draws the same line. It
/// is short because the line is evidence of where the user pointed: a circle of 40 pixels
/// drawn at three turns a second comes out one pixel smaller.
const EASE: Duration = Duration::from_millis(12);
/// How close to the line a kept point must be to go when the path fills, in logical pixels,
/// until a full path has too few points that close.
const TOLERANCE: f64 = 1.0;
/// The most points a sample leaves: one short of a path, so a release's own point fits.
const FULL: usize = MAX_PATH_POINTS - 1;
/// The most points a full path is thinned to.
const THINNED: usize = MAX_PATH_POINTS * 3 / 4;

/// Records a hold's pointer path in physical desktop pixels. It has no `Debug`: where the user
/// pointed is theirs.
pub struct PathRecorder {
    origin: PhysicalPoint,
    /// The display the line is drawn on. A point anywhere else is not kept.
    display: PhysicalRect,
    /// The dead zone's radius, in physical pixels.
    dead_zone: f64,
    /// The least distance between kept points, in physical pixels.
    spacing: f64,
    /// How close to the line a kept point must be to go when the path fills, in physical
    /// pixels. It doubles when a full path has too few points that close.
    tolerance: f64,
    /// Where the eased pointer is, and when a sample last moved it.
    eased: [f64; 2],
    seen: Option<Instant>,
    /// The pointer's last place on the display since the path started: where the line ends.
    end: Option<PhysicalPoint>,
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
            tolerance: TOLERANCE * scale,
            eased: origin.0.map(f64::from),
            seen: None,
            end: None,
            points: Vec::with_capacity(MAX_PATH_POINTS),
        }
    }

    /// Adds where the pointer is at `now`. True when a point was kept; [`Self::end`] follows
    /// the pointer either way.
    pub fn sample(&mut self, point: PhysicalPoint, now: Instant) -> bool {
        if !on(self.display, point) {
            return false;
        }
        // How much of the way to the pointer the eased point goes in the time since the last
        // sample. Nothing says how long the first one took to come, so it goes all the way.
        let follow = self.seen.replace(now).map_or(1.0, |seen| {
            let elapsed = now.saturating_duration_since(seen);
            1.0 - (-elapsed.as_secs_f64() / EASE.as_secs_f64()).exp()
        });
        let (last, started) = if let Some(last) = self.points.last() {
            (*last, false)
        } else {
            if distance_squared(self.origin, point) < self.dead_zone * self.dead_zone {
                return false;
            }
            // The line starts where the pointer was, when the display shows that place.
            let start = if on(self.display, self.origin) {
                self.origin
            } else {
                point
            };
            self.points.push(start);
            self.eased = start.0.map(f64::from);
            (start, true)
        };
        self.end = Some(point);
        for (eased, real) in self.eased.iter_mut().zip(point.0) {
            *eased += (f64::from(real) - *eased) * follow;
        }
        let eased = PhysicalPoint(self.eased.map(pixel));
        if distance_squared(last, eased) < self.spacing * self.spacing {
            return started;
        }
        self.points.push(eased);
        if self.points.len() > FULL {
            self.thin();
        }
        true
    }

    /// Makes room in a full path without stopping the line or moving it far: the points that
    /// go are the ones the line passes closest to, wherever they are along it, and the first
    /// and the newest stay. A path with too few of those gets twice the tolerance.
    fn thin(&mut self) {
        while self.points.len() > THINNED {
            let before = self.points.len();
            straighten(&mut self.points, self.tolerance);
            if before - self.points.len() < before / 8 {
                self.tolerance *= 2.0;
            }
        }
    }

    /// The display the line is drawn on, as given to [`Self::new`].
    #[must_use]
    pub const fn display(&self) -> PhysicalRect {
        self.display
    }

    /// The kept points, oldest first.
    pub fn points(&self) -> &[PhysicalPoint] {
        &self.points
    }

    /// Where the line ends: the pointer's last place on the display since the path started.
    /// The kept points trail it by the easing.
    #[must_use]
    pub const fn end(&self) -> Option<PhysicalPoint> {
        self.end
    }

    /// The path a release carries: empty when the pointer never left the dead zone, and
    /// otherwise ending at `pointer`, where the pointer itself was at release.
    pub fn finish(mut self, pointer: PhysicalPoint) -> PointerPath {
        let ends_elsewhere = self.points.last().is_some_and(|last| *last != pointer);
        if ends_elsewhere && on(self.display, pointer) {
            self.points.push(pointer);
        }
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

/// The pixel an eased place falls in.
#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "an eased place lies between desktop coordinates, which are i32"
)]
fn pixel(at: f64) -> i32 {
    at.round() as i32
}

/// Drops each point that lies within `tolerance` of the straight stretch from the kept point
/// before it to the point after it, which then stays. So one pass takes at most every second
/// point, moves the line by less than `tolerance`, and keeps the first and the last.
fn straighten(points: &mut Vec<PhysicalPoint>, tolerance: f64) {
    let (mut kept, mut next) = (1, 1);
    while next + 1 < points.len() {
        let [before, at, after] = [points[kept - 1], points[next], points[next + 1]];
        if off_stretch_squared(at, before, after) < tolerance * tolerance {
            points[kept] = after;
            next += 2;
        } else {
            points[kept] = at;
            next += 1;
        }
        kept += 1;
    }
    if next < points.len() {
        points[kept] = points[next];
        kept += 1;
    }
    points.truncate(kept);
}

/// How far `at` is from the straight stretch between `from` and `to`, squared. Past either
/// end it is the distance to that end, so the tip of a stroke that turns back is far from it.
fn off_stretch_squared(at: PhysicalPoint, from: PhysicalPoint, to: PhysicalPoint) -> f64 {
    let [x, y] = [0, 1].map(|axis| f64::from(at.0[axis]) - f64::from(from.0[axis]));
    let [dx, dy] = [0, 1].map(|axis| f64::from(to.0[axis]) - f64::from(from.0[axis]));
    let length = dx * dx + dy * dy;
    // Whole pixels: two different points are at least one apart.
    let along = if length < 0.5 {
        0.0
    } else {
        ((x * dx + y * dy) / length).clamp(0.0, 1.0)
    };
    let (off_x, off_y) = (x - along * dx, y - along * dy);
    off_x * off_x + off_y * off_y
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

    /// A recorder whose samples come a second apart, so the eased line has caught each one up
    /// before the next: what it keeps is then the samples themselves.
    struct Spaced {
        path: PathRecorder,
        at: Instant,
    }

    impl Spaced {
        fn new(path: PathRecorder) -> Self {
            Self {
                path,
                at: Instant::now(),
            }
        }

        fn sample(&mut self, point: PhysicalPoint) -> bool {
            self.at += Duration::from_secs(1);
            self.path.sample(point, self.at)
        }

        fn points(&self) -> &[PhysicalPoint] {
            self.path.points()
        }

        fn finish(self, pointer: PhysicalPoint) -> PointerPath {
            self.path.finish(pointer)
        }
    }

    fn recorder(scale: f32) -> Spaced {
        Spaced::new(PathRecorder::new(ORIGIN, scale, DISPLAY))
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
    fn a_full_straight_path_loses_every_second_point_and_keeps_its_first_and_newest() {
        // The path in order: where the pointer was, then a sample every two pixels.
        let point = |index: usize| match i32::try_from(index).expect("small") {
            0 => ORIGIN,
            index => at(22 + 2 * index, 0),
        };
        // One short of a path's 2,048 points: the most a sample leaves.
        let filled = || {
            let mut path = recorder(1.0);
            for index in 1..MAX_PATH_POINTS - 1 {
                assert!(path.sample(point(index)), "{index}");
            }
            assert_eq!(path.points().len(), MAX_PATH_POINTS - 1);
            path
        };
        // The room that is left is the release's.
        let released = filled().finish(at(0, 40));
        assert_eq!(released.0.len(), MAX_PATH_POINTS);
        assert_eq!(released.0.last(), Some(&at(0, 40)));
        assert_eq!(released.check(), Ok(()));

        let mut path = filled();
        assert!(path.sample(point(MAX_PATH_POINTS - 1)));
        let kept: Vec<_> = (0..MAX_PATH_POINTS - 1)
            .step_by(2)
            .map(point)
            .chain([point(MAX_PATH_POINTS - 1)])
            .collect();
        assert_eq!(kept.len(), 1_025);
        assert!(
            path.points() == kept,
            "every second point went, and the first and the newest stayed"
        );
        // The spacing is what it was.
        let PhysicalPoint([last, _]) = point(MAX_PATH_POINTS - 1);
        assert!(!path.sample(PhysicalPoint([last + 1, 0])));
        assert!(path.sample(PhysicalPoint([last + 2, 0])));
        assert_eq!(path.points().len(), 1_026);
    }

    #[test]
    fn a_full_path_keeps_every_corner() {
        // A sawtooth of straight strokes with a sample every four pixels: 120 pixels to the
        // right, then 60 down or up. Fifty teeth are 2,250 samples, so the path fills.
        let mut path = recorder(1.0);
        let (mut x, mut y) = (24, 0);
        let mut corners = Vec::new();
        let mut kept = usize::from(path.sample(at(x, y)));
        for tooth in 0..50 {
            for _ in 0..30 {
                x += 4;
                kept += usize::from(path.sample(at(x, y)));
            }
            corners.push(at(x, y));
            let down = if tooth % 2 == 0 { 4 } else { -4 };
            for _ in 0..15 {
                y += down;
                kept += usize::from(path.sample(at(x, y)));
            }
            corners.push(at(x, y));
        }
        assert_eq!(
            kept, 2_251,
            "every sample was far enough from the one before"
        );
        assert!(path.points().len() < MAX_PATH_POINTS);
        let turns: Vec<_> = path
            .points()
            .iter()
            .copied()
            .filter(|point| corners.contains(point))
            .collect();
        assert!(turns == corners, "a point the line turns at never goes");
    }

    #[test]
    fn a_path_of_nothing_but_turns_thins_at_the_least_tolerance_that_makes_room() {
        // Every sample is three pixels off the stretch between its neighbours, so none goes at
        // a tolerance of one pixel or of two, and every second one goes at four.
        let jog = |index: i32| at(24 + 3 * index, 3 * (index % 2));
        let mut path = recorder(1.0);
        for index in 0..2_046 {
            assert!(path.sample(jog(index)), "{index}");
        }
        assert_eq!(path.points().len(), MAX_PATH_POINTS - 1);
        assert!(path.sample(jog(2_046)));
        assert_eq!(path.path.tolerance.to_bits(), 4.0_f64.to_bits());
        assert_eq!(path.points().len(), 1_025);
        assert_eq!(path.points().first(), Some(&ORIGIN));
        assert_eq!(path.points().last(), Some(&jog(2_046)));
    }

    #[test]
    fn the_kept_line_trails_the_pointer_by_the_time_constant() {
        // The pointer jumps 100 pixels and rests. A lag of 12 ms has covered 1 - e^-k of the
        // way after k times 12 ms: 63.2, 86.5, 95.0, 98.2, 99.3 and 99.8 pixels.
        let start = Instant::now();
        let after = |steps: u32| start + Duration::from_millis(12) * steps;
        let mut path = PathRecorder::new(ORIGIN, 1.0, DISPLAY);
        assert!(!path.sample(ORIGIN, start));
        assert_eq!(path.end(), None, "no line yet");
        assert!(path.sample(at(100, 0), after(1)));
        assert_eq!(path.points(), [ORIGIN, at(63, 0)]);
        assert_eq!(path.end(), Some(at(100, 0)), "the line ends at the pointer");
        let kept: Vec<bool> = (2..=8)
            .map(|steps| path.sample(at(100, 0), after(steps)))
            .collect();
        // 99 is one pixel from 98, which is under the spacing; 100 is two from it. After
        // that a resting pointer keeps nothing.
        assert_eq!(kept, [true, true, true, false, true, false, false]);
        assert_eq!(
            path.points(),
            [
                ORIGIN,
                at(63, 0),
                at(86, 0),
                at(95, 0),
                at(98, 0),
                at(100, 0)
            ]
        );
        assert_eq!(path.end(), Some(at(100, 0)));
    }

    #[test]
    fn every_frame_rate_draws_the_same_circle_a_pixel_small() {
        // 40 pixels of radius at three turns a second. A lag of 12 ms that is read every dt
        // passes (1 - a) / |1 - a e^(-i w dt)| of a turning input, with a = e^(-dt / 12 ms):
        // 0.976 at 144 frames a second and 0.979 at 60, so 39.0 and 39.2 of the 40 pixels.
        let radius = |frames: u32| {
            let centre = at(1_000, 0);
            let start = Instant::now();
            let mut path = PathRecorder::new(ORIGIN, 1.0, DISPLAY);
            let mut settled = 0;
            for frame in 0..2 * frames {
                if frame == frames {
                    settled = path.points().len();
                }
                let seconds = f64::from(frame) / f64::from(frames);
                let turn = seconds * 3.0 * std::f64::consts::TAU;
                let [x, y] = centre.0;
                let point =
                    PhysicalPoint([x + pixel(40.0 * turn.cos()), y + pixel(40.0 * turn.sin())]);
                path.sample(point, start + Duration::from_secs_f64(seconds));
            }
            // The second second's points: the first has the line's start in it.
            let drawn = &path.points()[settled..];
            assert!(drawn.len() > 30, "{frames}");
            let far: f64 = drawn
                .iter()
                .map(|point| distance_squared(centre, *point).sqrt())
                .sum();
            far / f64::from(u32::try_from(drawn.len()).expect("small"))
        };
        let (fast, slow) = (radius(144), radius(60));
        assert!((38.8..39.3).contains(&fast), "{fast}");
        assert!((38.9..39.4).contains(&slow), "{slow}");
        assert!((fast - slow).abs() < 0.3, "{fast} {slow}");
    }

    #[test]
    fn a_release_ends_the_path_where_the_pointer_is_and_starts_none() {
        // Still inside the dead zone: nothing was drawn, so nothing is sent, wherever the
        // pointer is when the keys come up.
        let mut resting = recorder(1.0);
        assert!(!resting.sample(at(10, 0)));
        assert_eq!(resting.finish(at(300, 0)), PointerPath(Vec::new()));

        // One frame after a jump of 100 pixels the eased line has reached 63.
        let trailing = || {
            let start = Instant::now();
            let mut path = PathRecorder::new(ORIGIN, 1.0, DISPLAY);
            assert!(!path.sample(ORIGIN, start));
            assert!(path.sample(at(100, 0), start + Duration::from_millis(12)));
            path
        };
        assert_eq!(
            trailing().finish(at(100, 0)),
            PointerPath(vec![ORIGIN, at(63, 0), at(100, 0)])
        );
        assert_eq!(
            trailing().finish(at(104, 7)),
            PointerPath(vec![ORIGIN, at(63, 0), at(104, 7)]),
            "where the pointer is at release, not where a frame last read it"
        );
        assert_eq!(
            trailing().finish(at(63, 0)),
            PointerPath(vec![ORIGIN, at(63, 0)]),
            "not twice"
        );
        let [left, _] = DISPLAY.origin;
        assert_eq!(
            trailing().finish(PhysicalPoint([left - 1, 0])),
            PointerPath(vec![ORIGIN, at(63, 0)]),
            "only what the display shows"
        );
    }

    #[test]
    fn a_hold_of_any_length_fits_a_path() {
        let mut path = recorder(1.0);
        let mut kept = 0;
        for index in 0..60_000 {
            kept += usize::from(path.sample(at(24 + (index * 7) % 9_000, index % 400)));
            assert!(path.points().len() < MAX_PATH_POINTS, "{index}");
        }
        assert!(kept > 4 * MAX_PATH_POINTS, "the path filled more than once");
        // A pass takes at most every second point of a path over three quarters full.
        assert!(path.points().len() > MAX_PATH_POINTS / 3);
        assert_eq!(path.points().first(), Some(&ORIGIN));
        assert_eq!(path.finish(at(0, 40)).check(), Ok(()));
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
            path.finish(corners[1]),
            PointerPath(vec![ORIGIN, corners[0], corners[1]])
        );
    }

    #[test]
    fn a_path_starts_where_the_pointer_was_only_when_the_display_shows_that_place() {
        let [left, _] = DISPLAY.origin;
        let mut path = Spaced::new(PathRecorder::new(
            PhysicalPoint([left - 10, 0]),
            1.0,
            DISPLAY,
        ));
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
        assert_eq!(path.finish(at(-23, 0)), PointerPath(Vec::new()));
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
