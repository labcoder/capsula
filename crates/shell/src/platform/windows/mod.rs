//! Win32 implementations of the shell's platform functions.

mod pipe;
mod process;
mod window;

pub(crate) use pipe::{PipeClient, session_id};
pub(crate) use process::{launch_detached, lock_single_instance};
pub(crate) use window::{ShowError, show_without_activating};
