//! The performance counter, the clock the shell stamps its input with.

use std::time::Duration;

use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// How long ago `t_qpc`, a performance-counter stamp taken on this machine, was taken. None when
/// the counter can't be read or the stamp is later than now.
pub(crate) fn qpc_age(t_qpc: u64) -> Option<Duration> {
    let (mut now, mut frequency) = (0_i64, 0_i64);
    // SAFETY: `now` is writable for the call.
    if unsafe { QueryPerformanceCounter(&raw mut now) } == 0 {
        return None;
    }
    // SAFETY: `frequency` is writable for the call.
    if unsafe { QueryPerformanceFrequency(&raw mut frequency) } == 0 {
        return None;
    }
    age(now, frequency, t_qpc)
}

fn age(now: i64, frequency: i64, t_qpc: u64) -> Option<Duration> {
    let ticks = u64::try_from(now).ok()?.checked_sub(t_qpc)?;
    let frequency = u64::try_from(frequency)
        .ok()
        .filter(|&frequency| frequency > 0)?;
    let nanos = u128::from(ticks) * 1_000_000_000 / u128::from(frequency);
    Some(Duration::from_nanos(
        u64::try_from(nanos).unwrap_or(u64::MAX),
    ))
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
        assert_eq!(age(10_000_000, 0, 1), None);
        assert_eq!(age(-1, 10_000_000, 0), None);
    }
}
