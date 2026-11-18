//! Win32 implementations of the core's platform functions.

mod credential;
mod folders;
pub(super) mod microphone;
pub(super) mod pipe;
mod process;
pub(super) mod speaker;

#[expect(unused_imports, reason = "C7a-2b supplies the Accounts paste caller")]
pub(crate) use credential::WindowsKeyStageWriter;
#[cfg(test)]
pub(crate) use credential::assert_offline_native_header;
pub use folders::data_dir;
pub(crate) use pipe::PipeClient;
pub use pipe::session_id;
pub(crate) use process::launch_detached;
