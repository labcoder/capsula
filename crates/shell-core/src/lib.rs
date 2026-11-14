//! What Kumochi's shell knows and decides, apart from how it is drawn. No UI framework is in
//! this crate's dependency tree; `cargo xtask check-core` holds that.
#![expect(
    missing_docs,
    missing_debug_implementations,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
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
pub mod chat_notice;
pub mod claude_accounts;
pub mod credential;
pub mod drawing;
pub mod escape;
pub mod github_connections;
pub mod instance;
pub mod logging;
pub mod onboarding;
pub mod platform;
pub mod presence;
pub mod presenter;
pub mod runs;
pub mod settings;
pub mod sign_in_mark;
pub mod strings;
pub mod threads;
