//! How fast the stretch draws: the user's cap fitted to the display's refresh, so every frame
//! waits the same whole number of refreshes. How it's paced: docs/development/frame-rate.md.

use std::time::Duration;

use kumochi_protocol::message::MaxFrameRate;

/// Automatic's candidates.
const AUTOMATIC: [u16; 2] = [60, 72];

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
/// so a late or early vsync never adds or drops a refresh. With no known refresh, 0.8 of it.
pub(crate) fn min_gap(cap: MaxFrameRate, refresh: Option<Duration>) -> Duration {
    let interval = 1.0 / effective(cap, refresh);
    let slack = hertz(refresh).map_or(interval * 0.2, |hz| 0.5 / hz);
    Duration::from_secs_f64(interval - slack)
}

/// The refresh in hertz, if it's a real one. Windows reports whole hertz, so rounding gives
/// back exactly the rate it reported.
fn hertz(refresh: Option<Duration>) -> Option<f64> {
    refresh
        .map(|interval| (1.0 / interval.as_secs_f64()).round())
        .filter(|hz| hz.is_finite() && *hz >= 1.0)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kumochi_protocol::message::MaxFrameRate;

    use super::{effective, min_gap};

    fn hz(hz: f64) -> Duration {
        Duration::from_secs_f64(1.0 / hz)
    }

    fn cap(fps: u16) -> MaxFrameRate {
        MaxFrameRate::cap(fps).expect("a cap")
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
    fn with_no_refresh_the_cap_is_the_rate() {
        assert_eq!(effective(MaxFrameRate::AUTOMATIC, None), 60.0);
        assert_eq!(effective(cap(144), None), 144.0);
        assert_eq!(effective(cap(72), Some(Duration::ZERO)), 72.0);
        assert_eq!(min_gap(cap(72), None).as_micros(), 11_111);
        assert_eq!(min_gap(cap(72), Some(Duration::ZERO)).as_micros(), 11_111);
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
}
