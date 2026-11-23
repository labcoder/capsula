#![expect(unsafe_code, reason = "calls the macOS Mach clock API")]

use std::time::{Duration, Instant};

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> std::ffi::c_int;
}

fn counter() -> Option<(u64, Timebase)> {
    let mut timebase = Timebase { numer: 0, denom: 0 };
    // SAFETY: `timebase` has the SDK's layout and is writable for the call.
    if unsafe { mach_timebase_info(&raw mut timebase) } != 0 {
        return None;
    }
    // SAFETY: This call takes no arguments and reads the system counter.
    let now = unsafe { mach_continuous_time() };
    Some((now, timebase))
}

/// Stamp a future deadline on the system clock, sampling ticks before the Rust clock.
pub(crate) fn qpc_at(until: Instant) -> Option<u64> {
    let (now, timebase) = counter()?;
    let after = until.checked_duration_since(Instant::now())?;
    if after.is_zero() {
        return None;
    }
    stamp(now, &timebase, after)
}

/// Read the remaining budget from a stamp made anywhere on this boot.
pub(crate) fn qpc_left(deadline: u64) -> Option<Duration> {
    let (now, timebase) = counter()?;
    remaining(now, &timebase, deadline)
}

fn scale(value: u128, multiplier: u32, divisor: u32) -> Option<u128> {
    if multiplier == 0 {
        return None;
    }
    value
        .checked_mul(u128::from(multiplier))?
        .checked_div(u128::from(divisor))
}

fn stamp(now: u64, timebase: &Timebase, after: Duration) -> Option<u64> {
    let ticks = scale(after.as_nanos(), timebase.denom, timebase.numer)?;
    now.checked_add(u64::try_from(ticks).ok()?)
}

fn remaining(now: u64, timebase: &Timebase, deadline: u64) -> Option<Duration> {
    const NANOS_PER_SEC: u128 = 1_000_000_000;
    let ticks = deadline.checked_sub(now).filter(|&ticks| ticks != 0)?;
    let nanos = scale(u128::from(ticks), timebase.numer, timebase.denom)?;
    let secs = u64::try_from(nanos / NANOS_PER_SEC).ok()?;
    let subsec_nanos = u32::try_from(nanos % NANOS_PER_SEC).ok()?;
    Some(Duration::new(secs, subsec_nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_conversions_round_down() {
        let timebase = Timebase { numer: 5, denom: 2 };
        assert_eq!(stamp(10, &timebase, Duration::from_nanos(5)), Some(12));
        assert_eq!(stamp(10, &timebase, Duration::from_nanos(4)), Some(11));
        assert_eq!(stamp(10, &timebase, Duration::from_nanos(2)), Some(10));
        assert_eq!(remaining(10, &timebase, 12), Some(Duration::from_nanos(5)));
        assert_eq!(remaining(10, &timebase, 11), Some(Duration::from_nanos(2)));
        assert_eq!(remaining(10, &timebase, 10), None);
        assert_eq!(remaining(10, &timebase, 9), None);
    }

    #[test]
    fn unrepresentable_deadlines_and_invalid_timebases_are_refused() {
        let unit = Timebase { numer: 1, denom: 1 };
        assert_eq!(
            stamp(u64::MAX - 1, &unit, Duration::from_nanos(1)),
            Some(u64::MAX)
        );
        assert_eq!(stamp(u64::MAX, &unit, Duration::from_nanos(1)), None);
        assert_eq!(
            stamp(
                0,
                &unit,
                Duration::from_nanos(u64::MAX) + Duration::from_nanos(1)
            ),
            None
        );
        let wide = Timebase {
            numer: u32::MAX,
            denom: 1,
        };
        assert_eq!(
            remaining(0, &Timebase { numer: 2, denom: 1 }, u64::MAX),
            Some(Duration::new(36_893_488_147, 419_103_230))
        );
        assert_eq!(remaining(0, &wide, u64::MAX), None);
        for invalid in [
            Timebase { numer: 0, denom: 1 },
            Timebase { numer: 1, denom: 0 },
        ] {
            assert_eq!(stamp(10, &invalid, Duration::from_secs(1)), None);
            assert_eq!(remaining(10, &invalid, 11), None);
        }
    }

    #[test]
    fn a_child_reads_the_parents_deadline_on_the_same_clock() {
        const TEST_NAME: &str =
            "platform::macos::clock::tests::a_child_reads_the_parents_deadline_on_the_same_clock";
        const DEADLINE: &str = "TEST_SCREEN_CLOCK_DEADLINE";
        const READING: &str = "remaining_ns=";
        if let Some(deadline) = std::env::var_os(DEADLINE) {
            let stamp = deadline.to_str().unwrap().parse().unwrap();
            let left = qpc_left(stamp).expect("the parent's deadline is still ahead");
            println!("\n{READING}{}", left.as_nanos());
            return;
        }

        let now = Instant::now();
        assert_eq!(qpc_at(now), None);
        let (expired, _) = counter().expect("sample the clock before reading the expired stamp");
        assert_eq!(qpc_left(expired), None);
        let budget = Duration::from_secs(30);
        let stamp = qpc_at(now + budget).expect("stamp the future deadline");
        let before = qpc_left(stamp).expect("read before spawning the child");
        assert!(before > Duration::ZERO && before <= budget, "{before:?}");
        let child = {
            let _spawning = crate::platform::spawn_lock();
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env_clear()
                .env(DEADLINE, stamp.to_string())
                .output()
                .expect("run the headless clock child")
        };
        let after = qpc_left(stamp).unwrap_or_default();
        assert!(
            child.status.success(),
            "clock child exited with {}",
            child.status
        );
        let stdout = std::str::from_utf8(&child.stdout).unwrap();
        let sampled: u128 = stdout
            .lines()
            .find_map(|line| line.strip_prefix(READING))
            .expect("child reported its clock reading")
            .parse()
            .unwrap();
        assert!(
            (after.as_nanos()..=before.as_nanos()).contains(&sampled),
            "child read {sampled}ns outside the parent's {after:?}..={before:?}"
        );
    }
}
