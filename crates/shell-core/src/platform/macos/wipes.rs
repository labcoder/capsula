//! Test support: each dropped [`Secret`](super::Secret)'s address and whether it held only zeros
//! at the end of its drop, read from the heap backing itself just before it is freed. Secrets drop
//! on any test thread, so the log is shared; it never holds the bytes.

use std::sync::Mutex;

static LOG: Mutex<Vec<(usize, bool)>> = Mutex::new(Vec::new());

pub(super) fn record(bytes: &[u8]) {
    let entry = (bytes.as_ptr().addr(), bytes.iter().all(|&byte| byte == 0));
    LOG.lock().expect("the wipe log").push(entry);
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "only the core's own tests read the log")
)]
pub(super) fn len() -> usize {
    LOG.lock().expect("the wipe log").len()
}

/// Whether the first record of `addr` since `since` was all zeros.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "only the core's own tests read the log")
)]
pub(super) fn found(since: usize, addr: usize) -> Option<bool> {
    LOG.lock().expect("the wipe log")[since..]
        .iter()
        .find(|(at, _)| *at == addr)
        .map(|&(_, zero)| zero)
}
