use std::cell::Cell;
use std::rc::Rc;

use raw_window_handle::HasWindowHandle;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowDisplayAffinity, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    WINDOW_DISPLAY_AFFINITY,
};

use super::PresenceError;
use super::window::{hwnd_of, window_life};

#[derive(Debug)]
pub(crate) struct Affinity {
    pub(super) value: u32,
    destroyed: Rc<Cell<bool>>,
}

impl Affinity {
    pub(crate) fn gone(&self) -> bool {
        self.destroyed.get()
    }

    pub(super) fn apply(
        &self,
        hwnd: HWND,
        life: &Rc<Cell<bool>>,
        value: WINDOW_DISPLAY_AFFINITY,
    ) -> Result<(), PresenceError> {
        if self.gone() || !Rc::ptr_eq(&self.destroyed, life) || hwnd.is_invalid() {
            return Err(PresenceError::NotWin32);
        }
        // SAFETY: the live owner's destruction witness matches this receipt on the UI thread.
        unsafe { SetWindowDisplayAffinity(hwnd, value) }?;
        Ok(())
    }
}

pub(super) fn snapshot_affinity(
    hwnd: HWND,
    destroyed: Rc<Cell<bool>>,
) -> Result<Affinity, PresenceError> {
    if hwnd.is_invalid() || destroyed.get() {
        return Err(PresenceError::NotWin32);
    }
    let mut value = 0;
    // SAFETY: the current owner supplies its live window and writable DWORD.
    unsafe { GetWindowDisplayAffinity(hwnd, &raw mut value) }?;
    Ok(Affinity { value, destroyed })
}

pub(crate) fn window_affinity(window: &impl HasWindowHandle) -> Result<Affinity, PresenceError> {
    let hwnd = hwnd_of(window)?;
    snapshot_affinity(hwnd, window_life(hwnd)?)
}

pub(crate) fn exclude_window(
    window: &impl HasWindowHandle,
    prior: &Affinity,
) -> Result<(), PresenceError> {
    let hwnd = hwnd_of(window)?;
    prior.apply(hwnd, &window_life(hwnd)?, WDA_EXCLUDEFROMCAPTURE)
}

pub(crate) fn restore_window_affinity(
    window: &impl HasWindowHandle,
    prior: &Affinity,
) -> Result<(), PresenceError> {
    if prior.gone() {
        return Ok(());
    }
    let hwnd = hwnd_of(window)?;
    prior.apply(
        hwnd,
        &window_life(hwnd)?,
        WINDOW_DISPLAY_AFFINITY(prior.value),
    )
}
