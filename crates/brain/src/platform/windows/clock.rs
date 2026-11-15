//! The performance counter: the clock the shell stamps its input with, and the one the brain
//! stamps a capture's deadline with for the screen worker.

use std::time::{Duration, Instant};

use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// How long ago `t_qpc`, a performance-counter stamp taken on this machine, was taken. None when
/// the counter can't be read or the stamp is later than now.
pub(crate) fn qpc_age(t_qpc: u64) -> Option<Duration> {
    let (now, frequency) = counter()?;
    age(now, frequency, t_qpc)
}

/// The counter's stamp for `until`. The counter is read before the clock, so the stamp is never
/// later than `until`. None when the counter can't be read.
pub(crate) fn qpc_at(until: Instant) -> Option<u64> {
    let (now, frequency) = counter()?;
    stamp(
        now,
        frequency,
        until.saturating_duration_since(Instant::now()),
    )
}

/// How long until `stamp`, a stamp from [`qpc_at`] on this machine. None when the counter can't
/// be read or the stamp isn't later than now.
pub(crate) fn qpc_left(stamp: u64) -> Option<Duration> {
    let (now, frequency) = counter()?;
    left(now, frequency, stamp)
}

/// The counter now. None when it can't be read.
pub(crate) fn qpc_now() -> Option<u64> {
    counter().map(|(now, _)| now)
}

/// `span` in counter ticks, rounded up so a wait for it is never short. None when the counter
/// can't be read.
pub(crate) fn qpc_ticks(span: Duration) -> Option<u64> {
    let (_, frequency) = counter()?;
    ticks(span, frequency)
}

/// A counter stamp in the 100-ns units Windows gives a captured frame's draw time in. None when
/// the counter can't be read.
pub(crate) fn qpc_hns(stamp: u64) -> Option<u64> {
    let (_, frequency) = counter()?;
    hns(stamp, frequency)
}

fn counter() -> Option<(u64, u64)> {
    let (mut now, mut frequency) = (0_i64, 0_i64);
    // SAFETY: `now` is writable for the call.
    if unsafe { QueryPerformanceCounter(&raw mut now) } == 0 {
        return None;
    }
    // SAFETY: `frequency` is writable for the call.
    if unsafe { QueryPerformanceFrequency(&raw mut frequency) } == 0 {
        return None;
    }
    Some((
        u64::try_from(now).ok()?,
        u64::try_from(frequency)
            .ok()
            .filter(|&frequency| frequency > 0)?,
    ))
}

fn age(now: u64, frequency: u64, t_qpc: u64) -> Option<Duration> {
    Some(span(now.checked_sub(t_qpc)?, frequency))
}

fn left(now: u64, frequency: u64, stamp: u64) -> Option<Duration> {
    let ticks = stamp.checked_sub(now).filter(|&ticks| ticks > 0)?;
    Some(span(ticks, frequency))
}

/// Rounds down, so a deadline's stamp is never later than the deadline.
fn stamp(now: u64, frequency: u64, after: Duration) -> Option<u64> {
    let ticks = after.as_nanos() * u128::from(frequency) / 1_000_000_000;
    now.checked_add(u64::try_from(ticks).ok()?)
}

fn ticks(span: Duration, frequency: u64) -> Option<u64> {
    let nanos = span.as_nanos().checked_mul(u128::from(frequency))?;
    u64::try_from(nanos.div_ceil(1_000_000_000)).ok()
}

/// Rounds down, as Windows does for a frame's draw time.
fn hns(stamp: u64, frequency: u64) -> Option<u64> {
    u64::try_from(u128::from(stamp) * 10_000_000 / u128::from(frequency)).ok()
}

/// Rounds down, so time left is never overstated.
fn span(ticks: u64, frequency: u64) -> Duration {
    let nanos = u128::from(ticks) * 1_000_000_000 / u128::from(frequency);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamps_age_is_its_ticks_over_the_frequency_and_a_later_stamp_has_none() {
        assert_eq!(
            age(25_000_000, 10_000_000, 10_000_000),
            Some(Duration::from_millis(1_500))
        );
        assert_eq!(
            age(10_000_000, 10_000_000, 10_000_000),
            Some(Duration::ZERO)
        );
        assert_eq!(age(10_000_000, 10_000_000, 10_000_001), None);
    }

    #[test]
    fn a_deadline_stamps_round_down_and_one_now_or_past_has_nothing_left() {
        assert_eq!(
            stamp(10_000_000, 10_000_000, Duration::from_millis(1_500)),
            Some(25_000_000)
        );
        assert_eq!(
            stamp(10_000_000, 10_000_000, Duration::from_nanos(199)),
            Some(10_000_001)
        );
        assert_eq!(stamp(u64::MAX, 10_000_000, Duration::from_nanos(100)), None);
        assert_eq!(
            left(10_000_000, 10_000_000, 25_000_000),
            Some(Duration::from_millis(1_500))
        );
        assert_eq!(
            left(10_000_000, 3, 10_000_001),
            Some(Duration::from_nanos(333_333_333))
        );
        assert_eq!(left(10_000_000, 10_000_000, 10_000_000), None);
        assert_eq!(left(10_000_001, 10_000_000, 10_000_000), None);
    }

    #[test]
    fn a_span_in_ticks_rounds_up_and_a_stamp_in_frame_units_rounds_down() {
        assert_eq!(
            ticks(Duration::from_millis(100), 10_000_000),
            Some(1_000_000)
        );
        assert_eq!(ticks(Duration::from_nanos(1), 3), Some(1));
        assert_eq!(ticks(Duration::ZERO, 3), Some(0));
        assert_eq!(ticks(Duration::MAX, u64::MAX), None);
        assert_eq!(hns(25_000_000, 10_000_000), Some(25_000_000));
        assert_eq!(hns(1, 3), Some(3_333_333));
        assert_eq!(hns(3, 3), Some(10_000_000));
        assert_eq!(hns(u64::MAX, 1), None);
    }
}
