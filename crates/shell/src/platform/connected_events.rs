//! Events and roles the connected presence exchanges with its window backend.

use super::host::{HostChange, WatchGen};
use super::input::Button;

/// What the presence surfaces tell the UI thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(windows),
    expect(
        dead_code,
        reason = "native connected event producers exist only on Windows"
    )
)]
pub(crate) enum PresenceEvent {
    ShowStatus,
    DockPressed {
        qpc: i64,
    },
    /// The pointer entered or left the proxy or a presence window; ask `cursor_position`.
    /// `entered_qpc` is the counter when the entering move was dispatched, `None` on leave.
    PointerChanged {
        entered_qpc: Option<i64>,
    },
    /// The user picked a dock position in the tray menu, an index into `positions`.
    Position(usize),
    /// The user picked Retry in the tray menu.
    Retry,
    /// A panel close request hides the panel without quitting the shell.
    DismissPanel,
    /// Esc reached Kumochi: the global hotkey, a focused panel, or the Stop fallback.
    Escape,
    /// The local action observed speech on this exact pair and run.
    EscapeSpeech(crate::escape::SpeechPress),
    /// The user picked Quit in the tray menu.
    Quit,
    /// `--gate-observer` only: the harness asked for one dock repaint.
    #[cfg(any(test, feature = "gate-recorder"))]
    Repaint,
    /// A presence window lost mouse capture, stamped `seq` in the UI thread's input sequence.
    /// `released` is the button whose forwarded release caused it; `None` is a cancel mode or a
    /// capture taken from outside.
    CaptureLost {
        seq: u64,
        released: Option<Button>,
    },
    /// The outermost button release returned while a drained capture loss waited for it.
    Reconcile,
    /// A watched perch host changed; `w` is the watch's generation.
    HostChanged {
        w: WatchGen,
        change: HostChange,
    },
    /// A screen watch source set its watch's bit. It carries nothing: the presenter reads the
    /// bit.
    ScreenWoke,
}

/// Whether a GPUI surface paints only, owns status controls, or takes typing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresenceSurface {
    Overlay,
    Panel,
    /// The only surface Kumochi activates, and only when asked to.
    Text,
}

/// The localized text of the tray icon and its menu.
#[derive(Clone, Debug)]
#[cfg_attr(
    not(windows),
    expect(dead_code, reason = "only the native tray reads these labels")
)]
pub(crate) struct PresenceLabels {
    /// The tray icon's tooltip; Windows shows at most 127 UTF-16 units.
    pub(crate) tooltip: String,
    /// The title of the submenu that holds `positions`.
    pub(crate) position_menu: String,
    /// The nine dock positions, in the order `PresenceEvent::Position` indexes.
    pub(crate) positions: [String; 9],
    /// The Retry item.
    pub(crate) retry: String,
    /// The Quit item.
    pub(crate) quit: String,
}

/// What one mask update did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(windows),
    expect(dead_code, reason = "only the native hit mask emits updates")
)]
pub(crate) enum MaskUpdate {
    /// No pixel changed, so Windows wasn't called.
    Unchanged,
    /// Pixels changed in place.
    Shape,
    /// The proxy moved or resized, with its pixels, in the same call.
    Moved,
}

/// What the proxy saw, stamped with the performance counter when it dispatched it.
#[cfg(any(test, feature = "gate-recorder"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(windows),
    expect(dead_code, reason = "only the native proxy emits these notes")
)]
pub(crate) enum ProxyNote {
    /// A frame's `FRAME_DRAWN`, dispatched after the draw that posted it returned.
    Drawn { frame: u64, qpc: i64 },
    /// A left press carrying the session marker, at its physical screen point.
    Click { qpc: i64, point: [i32; 2] },
    /// The harness asked for one repaint.
    Repaint { qpc: i64 },
}

/// Receives the proxy's notes on the UI thread.
#[cfg(any(test, feature = "gate-recorder"))]
pub(crate) type Observe = Box<dyn Fn(ProxyNote)>;
