//! The authenticated shell id, held only for the lifetime of its IPC connection.

use std::sync::atomic::{AtomicU32, Ordering};

static PID: AtomicU32 = AtomicU32::new(0);

pub(crate) fn connected_shell_pid() -> u32 {
    PID.load(Ordering::Acquire)
}

#[cfg(any(windows, test))]
pub(super) fn same_client(control: u32, data: u32) -> bool {
    control != 0 && control == data
}

#[cfg(windows)]
pub(crate) fn register_shell(pid: u32) -> Registration<'static> {
    Registration::new(&PID, pid)
}

#[cfg(any(windows, test))]
pub(crate) struct Registration<'a> {
    slot: &'a AtomicU32,
    pid: u32,
}

#[cfg(any(windows, test))]
impl<'a> Registration<'a> {
    fn new(slot: &'a AtomicU32, pid: u32) -> Self {
        slot.store(pid, Ordering::Release);
        Self { slot, pid }
    }
}

#[cfg(any(windows, test))]
impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.slot
            .compare_exchange(self.pid, 0, Ordering::AcqRel, Ordering::Acquire)
            .unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_old_connection_cannot_clear_a_newer_shell_identity() {
        assert!(same_client(17, 17));
        assert!(!same_client(0, 0));
        assert!(!same_client(17, 23));
        let slot = AtomicU32::new(0);
        let old = Registration::new(&slot, 17);
        assert_eq!(slot.load(Ordering::Acquire), 17);
        let new = Registration::new(&slot, 23);
        drop(old);
        assert_eq!(slot.load(Ordering::Acquire), 23);
        drop(new);
        assert_eq!(slot.load(Ordering::Acquire), 0);
    }
}
