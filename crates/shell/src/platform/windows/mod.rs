//! Win32 implementations of the shell's platform functions.

mod pipe;
mod presence;
mod process;

pub(crate) use pipe::{PipeClient, session_id};
pub(crate) use presence::{
    PresenceError, PresenceEvent, PresenceLabels, PresenceNative, PresenceSurface,
    configure_presence_window, cursor_position, place_hidden_presence_window,
    place_presence_window, work_area,
};
pub(crate) use process::{launch_detached, lock_single_instance};
