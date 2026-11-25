//! The error and admission boundary for desktop-native presence surfaces.

#[cfg(not(windows))]
use raw_window_handle::HasWindowHandle;

/// Why a presence surface couldn't be set up or moved.
#[derive(Debug, thiserror::Error)]
pub enum PresenceError {
    /// The connected presence needs absolute physical placement and native visibility control.
    #[error("native connected-window geometry is unavailable on this platform")]
    GeometryUnavailable,
    /// No native owner can bind the connected presence's windows.
    #[error("native connected window binding is unavailable on this platform")]
    BindingsUnavailable,
    /// No native receipt can restore a connected panel's previous foreground.
    #[cfg(not(windows))]
    #[error("native connected foreground ownership is unavailable on this platform")]
    ForegroundUnavailable,
    /// GPUI gave no Win32 handle.
    #[cfg(windows)]
    #[error("no Win32 window handle")]
    NoHandle(#[from] raw_window_handle::HandleError),
    /// The handle wasn't a Win32 one.
    #[error("not a Win32 window")]
    NotWin32,
    /// A Win32 call failed.
    #[cfg(windows)]
    #[error("a Win32 call failed")]
    Os(#[from] windows::core::Error),
    /// The hover subclass couldn't be installed.
    #[cfg(windows)]
    #[error("can't subclass the window")]
    Subclass,
    /// The shell refused the tray icon.
    #[cfg(windows)]
    #[error("the tray refused the icon")]
    Tray,
    /// Raw Input was started twice.
    #[cfg(windows)]
    #[error("the keyboard is already armed")]
    KeyboardArmed,
    /// The native Escape lease couldn't be acquired.
    #[cfg(windows)]
    #[error("the Escape lease is unavailable")]
    EscapeLease,
    /// Session changes can't be watched, so a lock could leave keys held.
    #[cfg(windows)]
    #[error("can't watch session changes")]
    SessionWatch(#[source] windows::core::Error),
    /// Desktop switches can't be watched, so the secure desktop could leave keys held.
    #[cfg(windows)]
    #[error("can't watch desktop switches")]
    DesktopWatch,
    /// Windows kept the current foreground window.
    #[cfg(windows)]
    #[error("Windows refused to activate the window")]
    Activate,
    /// The hit mask was empty, couldn't be allocated, or was smaller than the proxy.
    #[cfg(windows)]
    #[error("the hit mask doesn't fit the proxy")]
    MaskSize,
    /// A frame id didn't fit a message parameter.
    #[cfg(windows)]
    #[error("the frame id doesn't fit a message")]
    FrameId,
    /// No selection surface was available for an input lease.
    #[error("no selection surface is bound")]
    SelectionSurface,
    /// A perch host's watch couldn't be installed, or two are already alive.
    #[cfg(windows)]
    #[error("can't watch the host window")]
    HostWatch,
}

/// The ordinary frontend selected by the platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frontend {
    /// The existing connected presence with its native input surfaces.
    Connected,
    /// The local disconnected window, without a native input owner.
    Local,
}

/// Select the frontend before any native fixture or window can start.
///
/// # Errors
/// The local window doesn't run the connected shell's command-line fixtures.
pub fn select_frontend(
    arguments: &mut std::iter::Peekable<impl Iterator>,
) -> std::io::Result<Frontend> {
    if !cfg!(target_os = "macos") {
        return Ok(Frontend::Connected);
    }
    if arguments.peek().is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the disconnected shell does not run connected launch modes",
        ));
    }
    Ok(Frontend::Local)
}

/// Require the native geometry backend before opening the connected presence's windows.
///
/// # Errors
/// Refuses platforms without the connected presence's native geometry backend.
pub fn require_connected_geometry() -> Result<(), PresenceError> {
    if cfg!(windows) {
        Ok(())
    } else {
        Err(PresenceError::GeometryUnavailable)
    }
}

/// Refuses the connected presence's absolute physical placement contract.
///
/// # Errors
/// No native backend is admitted here.
#[cfg(not(windows))]
pub fn place_presence_window(
    _: &mut impl HasWindowHandle,
    _: [i32; 4],
) -> Result<(), PresenceError> {
    Err(PresenceError::GeometryUnavailable)
}

/// Refuses the connected presence's native hide-in-place contract.
///
/// # Errors
/// No native backend is admitted here.
#[cfg(not(windows))]
pub fn hide_presence_window(_: &mut impl HasWindowHandle) -> Result<(), PresenceError> {
    Err(PresenceError::GeometryUnavailable)
}

/// Refuses to substitute a logical GPUI display area for a physical desktop work area.
///
/// # Errors
/// No native backend is admitted here.
#[cfg(not(windows))]
pub fn work_area() -> Result<[i32; 4], PresenceError> {
    Err(PresenceError::GeometryUnavailable)
}

/// Refuses to substitute a logical GPUI window origin for a physical client origin.
///
/// # Errors
/// No native backend is admitted here.
#[cfg(not(windows))]
pub fn client_origin(_: &impl HasWindowHandle) -> Result<[i32; 2], PresenceError> {
    Err(PresenceError::GeometryUnavailable)
}

#[cfg(all(test, not(windows)))]
mod tests {
    use std::cell::Cell;

    use raw_window_handle::{HandleError, WindowHandle};

    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn ordinary_mac_launch_selects_local_and_refuses_fixture_arguments() {
        assert_eq!(
            select_frontend(&mut std::iter::empty::<&str>().peekable()).unwrap(),
            Frontend::Local
        );
        assert_eq!(
            select_frontend(&mut std::iter::once("--esc-fixture").peekable())
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Unsupported
        );
    }

    #[derive(Default)]
    struct UnreadHandle(Cell<bool>);

    impl HasWindowHandle for UnreadHandle {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            self.0.set(true);
            Err(HandleError::Unavailable)
        }
    }

    #[test]
    fn connected_geometry_refuses_without_coordinates_or_handle_access() {
        let mut unopened = UnreadHandle::default();
        assert!(matches!(
            require_connected_geometry(),
            Err(PresenceError::GeometryUnavailable)
        ));
        assert!(matches!(
            place_presence_window(&mut unopened, [32, 64, 120, 72]),
            Err(PresenceError::GeometryUnavailable)
        ));
        assert!(matches!(
            hide_presence_window(&mut unopened),
            Err(PresenceError::GeometryUnavailable)
        ));
        assert!(matches!(
            work_area(),
            Err(PresenceError::GeometryUnavailable)
        ));
        assert!(matches!(
            client_origin(&unopened),
            Err(PresenceError::GeometryUnavailable)
        ));
        assert!(
            !unopened.0.get(),
            "unsupported geometry read the window handle"
        );
    }
}
