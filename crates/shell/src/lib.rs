//! Shared Kumochi shell views and their presentation setup.

/// Assets bundled with the shell.
pub mod assets;
mod local_dock;
pub mod strings;
/// The shell's product palette.
pub mod theme;

pub use local_dock::{OpenError as LocalDockOpenError, open as open_local_dock};
