//! What the brain and the shell share: product constants, secret handling, and the pipe frames
//! and messages. Data only: this crate makes no system calls.

pub mod constants;
pub mod frame;
pub mod geometry;
pub mod message;
pub mod profile;
mod secret;
pub mod tap;
pub mod voice;

pub use secret::Secret;

/// Identifies one build of both binaries: the version and the git commit. The brain refuses a
/// shell whose id differs, because the two only work as a matched pair.
pub const BUILD_ID: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KUMOCHI_COMMIT"));
