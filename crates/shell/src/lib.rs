//! Shared Kumochi shell views and their presentation setup.

/// Assets bundled with the shell.
pub mod assets;
mod local_dock;
#[path = "platform/presence_platform.rs"]
pub mod presence_platform;
#[path = "platform/qpc.rs"]
mod qpc;
pub mod strings;
/// The shell's product palette.
pub mod theme;

pub use local_dock::{
    Custody as LocalDockCustody, OpenError as LocalDockOpenError, open as open_local_dock,
    run as run_local_dock,
};
pub use qpc::QpcClock;
