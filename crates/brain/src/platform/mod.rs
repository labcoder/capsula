//! The operating system behind small, safe functions. Windows only for now.

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; the brain's only unsafe code lives here"
)]
mod windows;

#[cfg(windows)]
pub use windows::{
    Helper, Instance, Job, PipeServer, client_image, create_owner_only, launch_detached, session_id,
};
