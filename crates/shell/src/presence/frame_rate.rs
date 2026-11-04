//! How fast Kumochi animates: the user's cap fitted to the display's refresh, so every frame
//! waits the same whole number of refreshes. How it's paced: docs/development/frame-rate.md.

use std::time::{Duration, Instant};

use kumochi_protocol::message::MaxFrameRate;

/// Automatic's candidates.
const AUTOMATIC: [u16; 2] = [60, 72];

/// A frame steps at most this many effective intervals, so a stall can't fling what moves.
const MAX_STEP_FRAMES: f64 = 2.0;

/// The rate frames are drawn at: the display's refresh divided by the smallest whole number
/// that brings it to the cap or under. Automatic takes whichever of 60 and 72 comes out
/// higher, which is the one the refresh divides more evenly. With no known refresh, the cap.
pub(crate) fn effective(cap: MaxFrameRate, refresh: Option<Duration>) -> f64 {
    let Some(hz) = hertz(refresh) else {
        return f64::from(cap.fps().unwrap_or(AUTOMATIC[0]));
    };
    let fit = |cap: u16| hz / (hz / f64::from(cap)).ceil();
    match cap.fps() {
        Some(cap) => fit(cap),
        None => AUTOMATIC.into_iter().map(fit).fold(0.0, f64::max),
    }
}

/// How soon after a drawn frame the next may draw: the effective interval less half a refresh,
/// so a late or early vsync never adds or drops a refresh. With no known refresh, the whole
/// interval, so callbacks at any rate never draw faster than the cap.
pub(crate) fn min_gap(cap: MaxFrameRate, refresh: Option<Duration>) -> Duration {
    let interval = 1.0 / effective(cap, refresh);
    let slack = hertz(refresh).map_or(0.0, |hz| 0.5 / hz);
    Duration::from_secs_f64(interval - slack)
}

/// The refresh in hertz, if it's a real one. Windows reports whole hertz, so rounding gives
/// back exactly the rate it reported.
fn hertz(refresh: Option<Duration>) -> Option<f64> {
    refresh
        .map(|interval| (1.0 / interval.as_secs_f64()).round())
        .filter(|hz| hz.is_finite() && *hz >= 1.0)
}

/// Presence's one source of animation frames: the pacer, and whether a frame callback is armed.
/// Clients ask for frames; nothing else sets a timer.
#[derive(Debug, Default)]
pub(crate) struct FrameLoop {
    pacer: Pacer,
    armed: bool,
}

impl FrameLoop {
    /// Whether a callback must be armed now. An idle loop starts its pacer at `now`.
    pub(crate) fn want(&mut self, now: Instant) -> bool {
        if self.armed {
            return false;
        }
        self.armed = true;
        self.pacer.start(now);
        true
    }

    /// The seconds the callback at `now` steps, or `None` when it's too soon to draw.
    pub(crate) fn frame(
        &mut self,
        now: Instant,
        cap: MaxFrameRate,
        refresh: Option<Duration>,
    ) -> Option<f64> {
        self.pacer.frame(now, cap, refresh)
    }

    /// No client wants frames: the next want arms a callback again.
    pub(crate) fn idle(&mut self) {
        self.armed = false;
    }
}

/// An animation's frame clock: which vsync callbacks draw, and how far each drawn frame steps.
#[derive(Debug, Default)]
pub(crate) struct Pacer {
    /// When the loop started, or the last frame drew.
    last: Option<Instant>,
    /// Whether a frame drew since the loop started; the first callback always draws.
    drawn: bool,
    /// The last real refresh the display reported, kept through a callback that reports none.
    refresh: Option<Duration>,
}

impl Pacer {
    /// Starts the loop at `now`, so its first frame steps only the time since then.
    pub(crate) fn start(&mut self, now: Instant) {
        self.last = Some(now);
        self.drawn = false;
    }

    /// The seconds the callback at `now` steps, or `None` when it's too soon to draw.
    pub(crate) fn frame(
        &mut self,
        now: Instant,
        cap: MaxFrameRate,
        refresh: Option<Duration>,
    ) -> Option<f64> {
        if hertz(refresh).is_some() {
            self.refresh = refresh;
        }
        let since = self
            .last
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        if self.drawn && since < min_gap(cap, self.refresh) {
            return None;
        }
        self.last = Some(now);
        self.drawn = true;
        Some(
            since
                .as_secs_f64()
                .min(MAX_STEP_FRAMES / effective(cap, self.refresh)),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use kumochi_protocol::message::MaxFrameRate;

    use super::{Pacer, effective, min_gap};

    fn hz(hz: f64) -> Duration {
        Duration::from_secs_f64(1.0 / hz)
    }

    fn cap(fps: u16) -> MaxFrameRate {
        MaxFrameRate::cap(fps).expect("a cap")
    }

    fn ms(ms: f64) -> Duration {
        Duration::from_secs_f64(ms / 1000.0)
    }

    /// Frames drawn in two seconds of callbacks at 144 Hz, each reporting `refresh`.
    fn drawn_in_two_seconds(cap: MaxFrameRate, refresh: Option<Duration>) -> usize {
        let start = Instant::now();
        let mut pacer = Pacer::default();
        pacer.start(start);
        (1..=288)
            .filter(|&n| {
                let at = start + Duration::from_nanos(6_944_444 * n);
                pacer.frame(at, cap, refresh).is_some()
            })
            .count()
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "the rates are exact quotients")]
    fn the_effective_rate_divides_the_refresh_evenly() {
        let auto = MaxFrameRate::AUTOMATIC;
        assert_eq!(effective(auto, Some(hz(144.0))), 72.0);
        assert_eq!(effective(auto, Some(hz(120.0))), 60.0);
        assert_eq!(effective(auto, Some(hz(60.0))), 60.0);
        assert_eq!(effective(auto, Some(hz(59.0))), 59.0);
        assert_eq!(effective(auto, Some(hz(59.94))), 60.0);
        assert_eq!(effective(auto, Some(hz(165.0))), 55.0);
        assert_eq!(effective(auto, Some(hz(240.0))), 60.0);
        assert_eq!(effective(cap(144), Some(hz(144.0))), 144.0);
        assert_eq!(effective(cap(120), Some(hz(144.0))), 72.0);
        assert_eq!(effective(cap(72), Some(hz(144.0))), 72.0);
        assert_eq!(effective(cap(60), Some(hz(144.0))), 48.0);
        assert_eq!(effective(cap(30), Some(hz(144.0))), 28.8);
        assert_eq!(effective(cap(144), Some(hz(60.0))), 60.0);
        assert_eq!(effective(cap(30), Some(hz(60.0))), 30.0);
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "the rates are exact quotients")]
    fn with_no_refresh_the_cap_is_the_rate_and_the_whole_interval_the_gap() {
        assert_eq!(effective(MaxFrameRate::AUTOMATIC, None), 60.0);
        assert_eq!(effective(cap(144), None), 144.0);
        assert_eq!(effective(cap(72), Some(Duration::ZERO)), 72.0);
        assert_eq!(min_gap(cap(72), None).as_micros(), 13_888);
        assert_eq!(min_gap(cap(72), Some(Duration::ZERO)).as_micros(), 13_888);
    }

    #[test]
    fn the_gap_sits_half_a_refresh_before_the_frame_that_draws() {
        let micros = |fps: u16| min_gap(cap(fps), Some(hz(144.0))).as_micros();
        assert_eq!(micros(144), 3_472);
        assert_eq!(micros(72), 10_416);
        assert_eq!(micros(60), 17_361);
        // 0.8 of 28.8 fps lands exactly on the fourth refresh; half a refresh doesn't.
        assert_eq!(micros(30), 31_250);
        assert_eq!(min_gap(cap(144), Some(hz(60.0))).as_micros(), 8_333);
    }

    #[test]
    fn the_first_frame_and_a_restart_step_only_the_time_since_the_start() {
        for fps in [30, 144] {
            let start = Instant::now();
            let mut pacer = Pacer::default();
            pacer.start(start);
            let first = pacer.frame(start + ms(3.0), cap(fps), Some(hz(144.0)));
            assert_eq!(
                first.map(|step| (step * 1e6).round()),
                Some(3_000.0),
                "{fps}"
            );
            let restart = start + Duration::from_secs(5);
            pacer.start(restart);
            let first = pacer.frame(restart + ms(5.0), cap(fps), Some(hz(144.0)));
            assert_eq!(
                first.map(|step| (step * 1e6).round()),
                Some(5_000.0),
                "{fps}"
            );
        }
    }

    #[test]
    fn with_no_refresh_callbacks_at_144_hz_never_beat_the_cap() {
        assert_eq!(drawn_in_two_seconds(cap(30), None), 58);
        assert_eq!(drawn_in_two_seconds(cap(60), None), 96);
        assert_eq!(drawn_in_two_seconds(cap(120), None), 144);
        assert_eq!(drawn_in_two_seconds(cap(30), Some(hz(144.0))), 58);
    }

    #[test]
    fn a_callback_with_no_refresh_keeps_the_last_known_one() {
        let start = Instant::now();
        let early = start + ms(13.0);
        let mut unknown = Pacer::default();
        unknown.start(start);
        assert_eq!(unknown.frame(start, cap(72), None), Some(0.0));
        assert_eq!(
            unknown.frame(early, cap(72), None),
            None,
            "the whole interval"
        );
        let mut known = Pacer::default();
        known.start(start);
        assert_eq!(known.frame(start, cap(72), Some(hz(144.0))), Some(0.0));
        assert_eq!(
            known.frame(early, cap(72), None),
            Some(0.013),
            "144 Hz's slack"
        );
        assert_eq!(known.frame(early + ms(6.0), cap(72), None), None);
        assert_eq!(
            known.frame(early + ms(13.0), cap(72), Some(hz(144.0))),
            Some(0.013)
        );
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the steps are rounded to whole microseconds"
    )]
    fn a_frame_steps_its_whole_interval_and_a_stall_at_most_two() {
        let start = Instant::now();
        let mut pacer = Pacer::default();
        pacer.start(start);
        let refresh = Some(hz(61.0));
        assert_eq!(pacer.frame(start, cap(30), refresh), Some(0.0));
        let interval = 3.0 / 61.0;
        let next = start + Duration::from_secs_f64(interval);
        let step = pacer.frame(next, cap(30), refresh).expect("draws");
        assert_eq!((step * 1e6).round(), 49_180.0, "no slow motion at 61 Hz");
        let stalled = pacer
            .frame(next + Duration::from_secs(5), cap(30), refresh)
            .expect("draws");
        assert_eq!((stalled * 1e6).round(), 98_361.0);
    }
}
