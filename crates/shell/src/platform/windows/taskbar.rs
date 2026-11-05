//! The optional taskbar button has its own unowned, nonactivating zero-area window.

use super::presence::{PresenceEvent, send, struct_size};
use crate::settings::TaskbarChoice;
use async_channel::Sender;
use kumochi_protocol::constants::{DISPLAY_NAME, OBJECT_PREFIX};
use std::fmt;
use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SetLastError, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetWindowLongPtrW,
    IDI_APPLICATION, IsWindowVisible, LoadIconW, RegisterClassExW, RegisterWindowMessageW,
    SC_CLOSE, SC_MINIMIZE, SC_RESTORE, SW_HIDE, SW_SHOWNA, SetWindowLongPtrW, ShowWindow,
    UnregisterClassW, WA_INACTIVE, WM_ACTIVATE, WM_CLOSE, WM_SYSCOMMAND, WNDCLASSEXW,
    WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_POPUP, WS_SYSMENU,
};
use windows::core::{HSTRING, PCWSTR, w};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the taskbar window operation was refused")]
pub(crate) struct TaskbarError;

pub(crate) struct TaskbarButton {
    window: Option<NativeWindow>,
    events: Sender<PresenceEvent>,
}

impl fmt::Debug for TaskbarButton {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskbarButton")
            .field("shown", &self.window.is_some())
            .finish_non_exhaustive()
    }
}

impl TaskbarButton {
    pub(crate) fn new(events: Sender<PresenceEvent>) -> Self {
        Self {
            window: None,
            events,
        }
    }

    pub(crate) fn set(&mut self, choice: TaskbarChoice) -> Result<(), TaskbarError> {
        self.apply(choice, NativeWindow::new)
    }

    fn apply(
        &mut self,
        choice: TaskbarChoice,
        create: impl FnOnce(Sender<PresenceEvent>) -> Result<NativeWindow, TaskbarError>,
    ) -> Result<(), TaskbarError> {
        match choice {
            TaskbarChoice::Shown if self.window.is_none() => {
                self.window = Some(create(self.events.clone())?);
            }
            TaskbarChoice::Hidden => {
                if let Some(window) = &mut self.window {
                    window.destroy()?;
                }
                self.window = None;
            }
            TaskbarChoice::Shown => {}
        }
        Ok(())
    }
}

struct State {
    events: Sender<PresenceEvent>,
    taskbar_created: u32,
}

struct NativeWindow {
    instance: HINSTANCE,
    class: HSTRING,
    atom: u16,
    hwnd: HWND,
    state: Box<State>,
}

impl NativeWindow {
    fn new(events: Sender<PresenceEvent>) -> Result<Self, TaskbarError> {
        // SAFETY: reads this executable's module handle.
        let instance = unsafe { GetModuleHandleW(None) }
            .map_err(|_| TaskbarError)?
            .into();
        // SAFETY: registers a fixed, NUL-terminated notification name.
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        if taskbar_created == 0 {
            return Err(TaskbarError);
        }
        let mut native = Self {
            instance,
            class: HSTRING::from(format!("{OBJECT_PREFIX}-taskbar")),
            atom: 0,
            hwnd: HWND::default(),
            state: Box::new(State {
                events,
                taskbar_created,
            }),
        };
        // SAFETY: loads a shared stock icon, which must not be destroyed.
        let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.map_err(|_| TaskbarError)?;
        let class = WNDCLASSEXW {
            cbSize: struct_size::<WNDCLASSEXW>(),
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hIcon: icon,
            hIconSm: icon,
            lpszClassName: PCWSTR(native.class.as_ptr()),
            ..Default::default()
        };
        // SAFETY: the class and owned name outlive this call.
        native.atom = unsafe { RegisterClassExW(&raw const class) };
        if native.atom == 0 {
            return Err(TaskbarError);
        }
        // SAFETY: DEFAULTTOPRIMARY selects the primary monitor for this screen point.
        let monitor = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
        let mut info = MONITORINFO {
            cbSize: struct_size::<MONITORINFO>(),
            ..Default::default()
        };
        // SAFETY: info is sized writable storage for this live monitor.
        if !unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
            return Err(TaskbarError);
        }
        let title = HSTRING::from(DISPLAY_NAME);
        // shortcut: Alt-Tab has a blank preview; add a DWM iconic thumbnail if the owner wants one.
        // SAFETY: class/strings outlive the call; no owner, parent, menu or pixels are supplied.
        native.hwnd = unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(native.class.as_ptr()),
                &title,
                WS_POPUP | WS_SYSMENU,
                info.rcMonitor.left,
                info.rcMonitor.top,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .map_err(|_| TaskbarError)?;
        let state = (&raw const *native.state).expose_provenance().cast_signed();
        // SAFETY: clears this thread's error slot so a zero previous value is distinguishable from failure.
        unsafe { SetLastError(ERROR_SUCCESS) };
        // SAFETY: the boxed state remains stable until its UI-thread owner detaches it.
        let previous = unsafe { SetWindowLongPtrW(native.hwnd, GWLP_USERDATA, state) };
        // SAFETY: reads this thread's error slot immediately after the native operation.
        let error = unsafe { GetLastError() };
        if previous == 0 && error != ERROR_SUCCESS {
            return Err(TaskbarError);
        }
        // SAFETY: shows this thread's live zero-area window without activation.
        let _was_visible = unsafe { ShowWindow(native.hwnd, SW_SHOWNA) };
        // SAFETY: reads visibility of this thread's live window, not Explorer's taskbar button.
        if !unsafe { IsWindowVisible(native.hwnd) }.as_bool() {
            return Err(TaskbarError);
        }
        Ok(native)
    }

    fn destroy(&mut self) -> Result<(), TaskbarError> {
        // SAFETY: this thread owns the HWND; its callback state remains allocated.
        unsafe { DestroyWindow(self.hwnd) }.map_err(|_| TaskbarError)?;
        self.hwnd = HWND::default();
        Ok(())
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        if !self.hwnd.is_invalid() {
            // SAFETY: detaches the boxed state before freeing it, even if destruction fails.
            unsafe { SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0) };
            // SAFETY: this thread owns the window and no other path destroys it.
            if unsafe { DestroyWindow(self.hwnd) }.is_err() {
                tracing::warn!("can't destroy the taskbar window");
            }
        }
        if self.atom != 0 {
            // SAFETY: unregisters this owner's class after destroying its window.
            if unsafe { UnregisterClassW(PCWSTR(self.class.as_ptr()), Some(self.instance)) }
                .is_err()
            {
                tracing::warn!("can't unregister the taskbar window class");
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Status,
    Quit,
    Readd,
    Ignore,
    Default,
}

fn action(message: u32, value: usize, taskbar_created: u32) -> Action {
    match message {
        WM_CLOSE => Action::Quit,
        WM_ACTIVATE if u32::try_from(value & 0xffff).unwrap_or(WA_INACTIVE) != WA_INACTIVE => {
            Action::Status
        }
        WM_SYSCOMMAND => match u32::try_from(value & 0xfff0).unwrap_or(0) {
            SC_RESTORE | SC_MINIMIZE => Action::Status,
            SC_CLOSE => Action::Quit,
            _ => Action::Ignore,
        },
        _ if message == taskbar_created && taskbar_created != 0 => Action::Readd,
        _ => Action::Default,
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: reads our UI-thread slot, or zero during creation/destruction.
    let slot = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    let state = std::ptr::with_exposed_provenance::<State>(slot.cast_unsigned());
    if !state.is_null() {
        // SAFETY: the slot names the stable box until its UI-thread owner detaches it.
        let state = unsafe { &*state };
        match action(message, wp.0, state.taskbar_created) {
            Action::Status => send(&state.events, PresenceEvent::ShowStatus),
            Action::Quit => send(&state.events, PresenceEvent::Quit),
            Action::Readd => {
                // SAFETY: hides this live window before Explorer rediscovers its button.
                let _was_visible = unsafe { ShowWindow(hwnd, SW_HIDE) };
                // SAFETY: shows the same zero-area window without activation.
                let _was_visible = unsafe { ShowWindow(hwnd, SW_SHOWNA) };
            }
            Action::Ignore => {}
            Action::Default => {
                // SAFETY: forwards only messages outside intercepted command/close paths.
                return unsafe { DefWindowProcW(hwnd, message, wp, lp) };
            }
        }
        return LRESULT(0);
    }
    // SAFETY: forwards creation/destruction messages when no callback state is installed.
    unsafe { DefWindowProcW(hwnd, message, wp, lp) }
}

#[cfg(test)]
mod tests;
