use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuRadioItem, CreatePopupMenu, DestroyMenu, GetForegroundWindow, HMENU,
    MENU_ITEM_FLAGS, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, PostMessageW,
    SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_APP,
    WM_NULL,
};
use windows::core::HSTRING;

use super::{PresenceError, PresenceEvent, ProxyState, struct_size};

/// The tray's callback message to the proxy window.
pub(super) const TRAY_MESSAGE: u32 = WM_APP + 1;
/// The only icon this process puts in the tray.
pub(super) const TRAY_ID: u32 = 1;
/// Menu commands: positions are `1..=9`, so 0 stays "nothing picked".
const RETRY_COMMAND: usize = 10;
/// See [`RETRY_COMMAND`].
const QUIT_COMMAND: usize = 11;

/// Adds the tray icon and opts into version 4 callbacks, which report keyboard use too.
pub(super) fn add_tray(hwnd: HWND, state: &ProxyState) -> Result<(), PresenceError> {
    let mut data = NOTIFYICONDATAW {
        cbSize: struct_size::<NOTIFYICONDATAW>(),
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: TRAY_MESSAGE,
        hIcon: state.icon,
        ..Default::default()
    };
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let tip = state
        .labels
        .tooltip
        .encode_utf16()
        .take(data.szTip.len() - 1);
    for (slot, unit) in data.szTip.iter_mut().zip(tip) {
        *slot = unit;
    }
    // SAFETY: `data` is valid for the call.
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &raw const data) }.as_bool() {
        return Err(PresenceError::Tray);
    }
    // SAFETY: as above, for the icon just added.
    if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &raw const data) }.as_bool() {
        // SAFETY: removes the icon this function added before its version request failed.
        if !unsafe { Shell_NotifyIconW(NIM_DELETE, &raw const data) }.as_bool() {
            tracing::warn!("can't remove the partially initialized tray icon");
        }
        return Err(PresenceError::Tray);
    }
    Ok(())
}

/// Owns a popup menu and destroys it, with any submenu attached to it.
#[derive(Debug)]
struct Menu(HMENU);

impl Menu {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: plain call; the guard owns the result.
        Ok(Self(unsafe { CreatePopupMenu() }?))
    }

    fn append(&self, flags: MENU_ITEM_FLAGS, id: usize, label: &str) -> windows::core::Result<()> {
        // SAFETY: the menu is live and the label outlives the call.
        unsafe { AppendMenuW(self.0, flags, id, &HSTRING::from(label)) }
    }
}

impl Drop for Menu {
    fn drop(&mut self) {
        // SAFETY: the menu is live and owned only by this guard.
        if let Err(error) = unsafe { DestroyMenu(self.0) } {
            tracing::debug!(%error, "can't destroy a menu");
        }
    }
}

/// Shows the tray menu at `[x, y]` and returns what the user picked, if anything.
pub(super) fn pick_from_tray_menu(
    hwnd: HWND,
    state: &ProxyState,
    [x, y]: [i32; 2],
) -> windows::core::Result<Option<PresenceEvent>> {
    let labels = &state.labels;
    let menu = Menu::new()?;
    let positions = Menu::new()?;
    for (index, label) in labels.positions.iter().enumerate() {
        positions.append(MF_STRING, index + 1, label)?;
    }
    let selected = u32::try_from(state.position.get() + 1).unwrap_or(2);
    // SAFETY: the submenu owns command IDs 1 through 9, and flag 0 selects by command ID.
    unsafe { CheckMenuRadioItem(positions.0, 1, 9, selected, 0) }?;
    menu.append(
        MF_POPUP,
        positions.0.0.expose_provenance(),
        &labels.position_menu,
    )?;
    // The parent menu destroys the submenu from here on.
    std::mem::forget(positions);
    menu.append(MF_SEPARATOR, 0, "")?;
    let retry_flags = if state.retry_enabled.get() {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    menu.append(retry_flags, RETRY_COMMAND, &labels.retry)?;
    menu.append(MF_STRING, QUIT_COMMAND, &labels.quit)?;

    // SAFETY: reads the foreground handle without transferring ownership.
    let previous = unsafe { GetForegroundWindow() };
    // Without the foreground, the menu wouldn't close when the user clicks elsewhere.
    // SAFETY: `hwnd` is this thread's live window.
    if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
        tracing::debug!("the tray menu didn't get the foreground");
    }
    let flags = TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON;
    // SAFETY: the menu and `hwnd` are live; it returns the picked command, 0 for none.
    let command = unsafe { TrackPopupMenuEx(menu.0, flags.0, x, y, hwnd, None) }.0;
    // Lets the menu close cleanly on the next click, as Win32 tray menus need.
    // SAFETY: `hwnd` is live; WM_NULL does nothing.
    if let Err(error) = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) } {
        tracing::debug!(%error, "can't nudge the tray menu closed");
    }
    // A click elsewhere already chose a new foreground; never override that choice.
    // SAFETY: reads the current foreground handle.
    let current = unsafe { GetForegroundWindow() };
    if current == hwnd && previous != hwnd && !previous.is_invalid() {
        // SAFETY: this only requests restoration of the foreground we temporarily replaced.
        if !unsafe { SetForegroundWindow(previous) }.as_bool() {
            tracing::debug!("can't restore focus after the tray menu");
        }
    }
    Ok(match usize::try_from(command).unwrap_or(0) {
        position @ 1..=9 => Some(PresenceEvent::Position(position - 1)),
        RETRY_COMMAND => Some(PresenceEvent::Retry),
        QUIT_COMMAND => Some(PresenceEvent::Quit),
        _ => None,
    })
}
