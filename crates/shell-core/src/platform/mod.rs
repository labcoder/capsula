//! The operating system behind the models: the plain host and input types, and the calls the
//! models make themselves (folders, the credential store, the microphone). Windows only for now.

pub mod credential;
pub mod host;
pub mod input;
pub mod microphone;

#[cfg(windows)]
#[expect(
    unsafe_code,
    reason = "Win32 calls; the core's only shipped unsafe code lives here"
)]
mod windows;

#[cfg(windows)]
pub use windows::data_dir;
