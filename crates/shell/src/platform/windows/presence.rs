//! Win32 presence windows, input proxy, tray, capture exclusion and focus.

mod capture;
mod focus;
mod proxy;
mod window;

#[cfg(test)]
mod tests;

use async_channel::Sender;
use windows::Win32::System::Performance::QueryPerformanceCounter;

pub(crate) use crate::platform::{PresenceError, PresenceEvent, PresenceLabels, PresenceSurface};

pub(crate) use capture::{Affinity, exclude_window, restore_window_affinity, window_affinity};
pub(super) use focus::process_of;
pub(crate) use focus::{Foreground, activate_presence_window, restore_foreground};
pub(crate) use proxy::PresenceNative;
pub(crate) use window::{
    configure_presence_window, cursor_position, hide_presence_window, place_presence_window,
    work_area,
};
pub(super) use window::{foreground_owns_escape, hwnd_of};

/// The performance counter, the clock `PresentMon`'s `--qpc_time` reports in.
pub(crate) fn performance_counter() -> i64 {
    let mut counter = 0;
    // SAFETY: `counter` is writable; the call can't fail on Windows XP or later.
    if let Err(error) = unsafe { QueryPerformanceCounter(&raw mut counter) } {
        tracing::debug!(%error, "can't read the performance counter");
    }
    counter
}

/// Queues an event for the UI thread; the queue is unbounded, so this never blocks.
pub(super) fn send(events: &Sender<PresenceEvent>, event: PresenceEvent) {
    if events.try_send(event).is_err() {
        tracing::debug!("the UI stopped listening to presence events");
    }
}

/// The signed low and high words Win32 packs into one parameter.
pub(super) fn signed_words(value: usize) -> [i32; 2] {
    let word = |shift: u32| {
        u16::try_from((value >> shift) & 0xFFFF)
            .unwrap_or(0)
            .cast_signed()
    };
    [i32::from(word(0)), i32::from(word(16))]
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    reason = "Win32 structs are a few hundred bytes"
)]
pub(super) const fn struct_size<T>() -> u32 {
    size_of::<T>() as u32
}
