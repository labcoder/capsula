#![expect(unsafe_code, reason = "reads the macOS continuous clock")]

use std::time::Duration;

const CLOCK_MONOTONIC_RAW: std::ffi::c_uint = 4;

unsafe extern "C" {
    fn clock_gettime_nsec_np(clock_id: std::ffi::c_uint) -> u64;
}

fn counter() -> Option<u64> {
    // SAFETY: This clock id is valid on Darwin; the call takes no pointers.
    let now = unsafe { clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW) };
    read_sample(now)
}

fn read_sample(now: u64) -> Option<u64> {
    (now != 0).then_some(now)
}

/// Stamp the budget in continuous nanoseconds. Parity with Windows is at admission,
/// not in the stamp's units: zero or elapsed budgets refuse, and an unrepresentable stamp is absent.
pub(crate) fn qpc_at(budget: Duration) -> Option<u64> {
    stamp(counter()?, budget)
}

/// Read the remaining budget, or refuse a stamp at or before now.
pub(crate) fn qpc_left(deadline: u64) -> Option<Duration> {
    deadline
        .checked_sub(counter()?)
        .filter(|&nanos| nanos != 0)
        .map(Duration::from_nanos)
}

fn stamp(now: u64, after: Duration) -> Option<u64> {
    now.checked_add(u64::try_from(after.as_nanos()).ok()?)
}

#[cfg(test)]
mod tests;
