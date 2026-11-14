//! What Kumochi's shell knows and decides, apart from how it is drawn. No UI framework is in
//! this crate's dependency tree; `cargo xtask check-core` holds that.
#![expect(
    missing_docs,
    missing_debug_implementations,
    reason = "these models became public by moving out of the shell; they get docs and Debug \
              when the controller's public surface is designed"
)]

#[cfg(test)]
#[path = "../../scene/tests/support/allocation.rs"]
#[expect(
    dead_code,
    reason = "shared allocator helpers serve several test binaries"
)]
mod allocation;

pub mod accounts;
pub mod claude_accounts;
pub mod credential;
pub mod escape;
pub mod instance;
pub mod platform;
pub mod presence;
pub mod presenter;
pub mod sign_in_mark;
