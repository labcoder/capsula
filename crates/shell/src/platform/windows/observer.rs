//! The native half of `--gate-observer`: the hit proxy stamps the gate harness's messages and
//! tagged clicks the moment it dispatches them, and the dock's paint posts it each frame's id.
//! Without an observer the proxy ignores all three, so normal runs behave as before.

use capsula_protocol::constants::{INPUT_SESSION_MARKER, OBSERVER_REPAINT_MESSAGE};
use gpui_kit::Window;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageExtraInfo, GetMessagePos, PostMessageW, WM_APP, WM_LBUTTONDOWN,
};

use super::presence::{PresenceError, hwnd_of, performance_counter, signed_words};

/// Posted by the dock's paint to the proxy with the frame id; private to the shell.
const FRAME_DRAWN: u32 = WM_APP + 0x11;

/// What the proxy saw, stamped with the performance counter when it dispatched it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProxyNote {
    /// A frame's `FRAME_DRAWN`, dispatched after the draw that posted it returned.
    Drawn { frame: u64, qpc: i64 },
    /// A left press carrying the session marker, at its physical screen point.
    Click { qpc: i64, point: [i32; 2] },
    /// The harness asked for one repaint.
    Repaint { qpc: i64 },
}

/// Receives the proxy's notes on the UI thread.
pub(crate) type Observe = Box<dyn Fn(ProxyNote)>;

/// The proxy, for posting frame ids to; only the UI thread holds it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProxyHandle(pub(super) HWND);

impl ProxyHandle {
    /// Queues `frame`'s completion behind the message being dispatched now, so the proxy
    /// sees it only after the draw and present that run inside that message return.
    ///
    /// # Errors
    /// Fails when Windows refuses the post.
    pub(crate) fn post_drawn(self, frame: u64) -> Result<(), PresenceError> {
        let frame = usize::try_from(frame).map_err(|_| PresenceError::FrameId)?;
        // SAFETY: posts plain integers to this thread's live proxy.
        unsafe { PostMessageW(Some(self.0), FRAME_DRAWN, WPARAM(frame), LPARAM(0)) }?;
        Ok(())
    }

    /// The handle as the integer the harness reads.
    pub(crate) fn value(self) -> usize {
        self.0.0.expose_provenance()
    }
}

/// Stamps and reports the observer's messages. True means the message is consumed; a
/// click always goes on to the proxy's normal handling.
pub(super) fn on_message(
    observe: Option<&Observe>,
    message: u32,
    wparam: WPARAM,
    click_qpc: Option<i64>,
) -> bool {
    let Some(observe) = observe else {
        return false;
    };
    let qpc = click_qpc.unwrap_or_else(performance_counter);
    match message {
        FRAME_DRAWN => {
            let frame = u64::try_from(wparam.0).unwrap_or(u64::MAX);
            observe(ProxyNote::Drawn { frame, qpc });
            true
        }
        OBSERVER_REPAINT_MESSAGE => {
            observe(ProxyNote::Repaint { qpc });
            true
        }
        WM_LBUTTONDOWN => {
            // SAFETY: reads this thread's last retrieved message's extra info.
            let extra = unsafe { GetMessageExtraInfo() };
            if extra.0.cast_unsigned() == INPUT_SESSION_MARKER {
                // The screen point Windows routed the press at, so a proxy moved since can't
                // shift it the way converting the client point with today's origin would.
                // SAFETY: reads this thread's last retrieved message's cursor position.
                let position = unsafe { GetMessagePos() };
                observe(ProxyNote::Click {
                    qpc,
                    point: signed_words(usize::try_from(position).unwrap_or(0)),
                });
            }
            false
        }
        _ => false,
    }
}

/// Counts per second of [`performance_counter`].
pub(crate) fn performance_frequency() -> i64 {
    let mut frequency = 0;
    // SAFETY: `frequency` is writable; the call can't fail on Windows XP or later.
    if let Err(error) = unsafe { QueryPerformanceFrequency(&raw mut frequency) } {
        tracing::debug!(%error, "can't read the performance counter's frequency");
    }
    frequency
}

/// A GPUI window's client top-left on screen, in physical pixels.
///
/// # Errors
/// Fails when the window has no Win32 handle or Windows can't map the point.
pub(crate) fn client_origin(window: &Window) -> Result<[i32; 2], PresenceError> {
    let mut point = POINT::default();
    // SAFETY: the handle is the window's live HWND and `point` is writable.
    unsafe { ClientToScreen(hwnd_of(window)?, &raw mut point) }.ok()?;
    Ok([point.x, point.y])
}

/// A GPUI window's Win32 handle as the integer the harness reads.
///
/// # Errors
/// Fails when the window has no Win32 handle.
pub(crate) fn window_handle(window: &Window) -> Result<usize, PresenceError> {
    Ok(hwnd_of(window)?.0.expose_provenance())
}
